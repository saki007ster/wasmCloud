use std::env::temp_dir;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context as _};
use oci_distribution::client::ClientProtocol;
use oci_distribution::client::ImageData;
use oci_distribution::Reference;
use oci_wasm::WASM_LAYER_MEDIA_TYPE;
use oci_wasm::WASM_MANIFEST_MEDIA_TYPE;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use wascap::jwt;

use crate::tls;
use crate::RegistryConfig;

const PROVIDER_ARCHIVE_MEDIA_TYPE: &str = "application/vnd.wasmcloud.provider.archive.layer.v1+par";
const WASM_MEDIA_TYPE: &str = "application/vnd.module.wasm.content.layer.v1+wasm";
const OCI_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

/// Whether to update an OCI artifact cache
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OciArtifactCacheUpdate {
    /// Do not update the OCI artifact cache
    #[default]
    Ignore,
    /// Update the cache
    Update,
}

/// OCI artifact fetcher
#[derive(Clone, Debug)]
pub struct OciFetcher {
    additional_ca_paths: Vec<PathBuf>,
    allow_latest: bool,
    allow_insecure: bool,
    auth: oci_distribution::secrets::RegistryAuth,
}

impl Default for OciFetcher {
    fn default() -> Self {
        Self {
            additional_ca_paths: Vec::default(),
            allow_latest: false,
            allow_insecure: false,
            auth: oci_distribution::secrets::RegistryAuth::Anonymous,
        }
    }
}

impl From<&RegistryConfig> for OciFetcher {
    fn from(
        RegistryConfig {
            auth,
            allow_latest,
            allow_insecure,
            additional_ca_paths,
            ..
        }: &RegistryConfig,
    ) -> Self {
        Self {
            auth: auth.into(),
            allow_latest: *allow_latest,
            allow_insecure: *allow_insecure,
            additional_ca_paths: additional_ca_paths.clone(),
        }
    }
}

impl From<RegistryConfig> for OciFetcher {
    fn from(
        RegistryConfig {
            auth,
            allow_latest,
            allow_insecure,
            additional_ca_paths,
            ..
        }: RegistryConfig,
    ) -> Self {
        Self {
            auth: auth.into(),
            allow_latest,
            allow_insecure,
            additional_ca_paths,
        }
    }
}

/// Default directory in which OCI artifacts are cached
pub async fn oci_cache_dir() -> anyhow::Result<PathBuf> {
    let path = temp_dir().join("wasmcloud_ocicache");
    if !fs::try_exists(&path).await? {
        fs::create_dir_all(&path).await?;
    }
    Ok(path)
}

#[allow(unused)]
async fn cache_oci_image(
    image: ImageData,
    cache_filepath: impl AsRef<Path>,
    digest_filepath: impl AsRef<Path>,
) -> std::io::Result<()> {
    let mut cache_file = fs::File::create(cache_filepath).await?;
    let content = image
        .layers
        .into_iter()
        .flat_map(|l| l.data)
        .collect::<Vec<_>>();
    cache_file.write_all(&content).await?;
    cache_file.flush().await?;
    if let Some(digest) = image.digest {
        let mut digest_file = fs::File::create(digest_filepath).await?;
        digest_file.write_all(digest.as_bytes()).await?;
        digest_file.flush().await?;
    }
    Ok(())
}

fn prune_filepath(img: &str) -> String {
    let mut img = img.replace(':', "_");
    img = img.replace('/', "_");
    img = img.replace('.', "_");
    img
}

impl OciFetcher {
    /// Fetch an OCI path
    pub async fn fetch_path(
        &self,
        output_dir: impl AsRef<Path>,
        img: impl AsRef<str>,
        accepted_media_types: Vec<&str>,
        cache: OciArtifactCacheUpdate,
    ) -> anyhow::Result<PathBuf> {
        let output_dir = output_dir.as_ref();
        let img = img.as_ref().to_lowercase(); // the OCI spec does not allow for capital letters in references
        if !self.allow_latest && img.ends_with(":latest") {
            bail!("fetching images tagged 'latest' is currently prohibited in this host. This option can be overridden with WASMCLOUD_OCI_ALLOW_LATEST")
        }
        let pruned_filepath = prune_filepath(&img);
        let cache_file = output_dir.join(&pruned_filepath);
        let mut digest_file = output_dir.join(&pruned_filepath).clone();
        digest_file.set_extension("digest");

        let img = Reference::from_str(&img)?;

        let protocol = if self.allow_insecure {
            ClientProtocol::HttpsExcept(vec![img.registry().to_string()])
        } else {
            ClientProtocol::Https
        };
        let mut certs = tls::NATIVE_ROOTS_OCI.to_vec();
        if !self.additional_ca_paths.is_empty() {
            certs.extend(
                tls::load_certs_from_paths(&self.additional_ca_paths)
                    .context("failed to load CA certs from provided paths")?
                    .iter()
                    .map(|cert| oci_distribution::client::Certificate {
                        encoding: oci_distribution::client::CertificateEncoding::Der,
                        data: cert.to_vec(),
                    }),
            );
        }
        let c = oci_distribution::Client::new(oci_distribution::client::ClientConfig {
            protocol,
            extra_root_certificates: certs,
            ..Default::default()
        });

        // In case of a cache miss where the file does not exist, pull a fresh OCI Image
        if fs::metadata(&cache_file).await.is_ok() {
            let (_, oci_digest) = c
                .pull_manifest(&img, &self.auth)
                .await
                .context("failed to fetch OCI manifest")?;
            // If the digest file doesn't exist that is ok, we just unwrap to an empty string
            let file_digest = fs::read_to_string(&digest_file).await.unwrap_or_default();
            if !oci_digest.is_empty() && !file_digest.is_empty() && file_digest == oci_digest {
                return Ok(cache_file);
            }
        }

        let imgdata = c
            .pull(&img, &self.auth, accepted_media_types)
            .await
            .context("failed to fetch OCI bytes")?;
        // As a client, we should reject invalid OCI artifacts
        if imgdata
            .manifest
            .as_ref()
            .map(|m| m.media_type.as_deref().unwrap_or_default() == WASM_MANIFEST_MEDIA_TYPE)
            .unwrap_or(false)
            && imgdata.layers.len() > 1
        {
            bail!(
                "Found invalid OCI wasm artifact, expected single layer, found {} layers",
                imgdata.layers.len()
            )
        }
        // Update the OCI artifact cache if specified
        if let OciArtifactCacheUpdate::Update = cache {
            cache_oci_image(imgdata, &cache_file, digest_file)
                .await
                .context("failed to cache OCI bytes")?;
        }

        Ok(cache_file)
    }

    /// Fetch component from OCI
    ///
    /// # Errors
    ///
    /// Returns an error if either fetching fails or reading the fetched OCI path fails
    pub async fn fetch_component(&self, oci_ref: impl AsRef<str>) -> anyhow::Result<Vec<u8>> {
        let path = self
            .fetch_path(
                oci_cache_dir().await?,
                oci_ref,
                vec![WASM_MEDIA_TYPE, OCI_MEDIA_TYPE, WASM_LAYER_MEDIA_TYPE],
                OciArtifactCacheUpdate::Update,
            )
            .await
            .context("failed to fetch OCI path")?;
        fs::read(&path)
            .await
            .with_context(|| format!("failed to read `{}`", path.display()))
    }

    /// Fetch provider from OCI
    ///
    /// # Errors
    ///
    /// Returns an error if either fetching fails or reading the fetched OCI path fails
    pub async fn fetch_provider(
        &self,
        oci_ref: impl AsRef<str>,
        host_id: impl AsRef<str>,
    ) -> anyhow::Result<(PathBuf, Option<jwt::Token<jwt::CapabilityProvider>>)> {
        let path = self
            .fetch_path(
                oci_cache_dir().await?,
                oci_ref.as_ref(),
                vec![PROVIDER_ARCHIVE_MEDIA_TYPE, OCI_MEDIA_TYPE],
                OciArtifactCacheUpdate::Update,
            )
            .await
            .context("failed to fetch OCI path")?;
        crate::par::read(&path, host_id, oci_ref, crate::par::UseParFileCache::Use)
            .await
            .with_context(|| format!("failed to read `{}`", path.display()))
    }

    /// Used to set additional CA paths that will be used as part of fetching components and providers
    pub fn with_additional_ca_paths(mut self, paths: &[impl AsRef<Path>]) -> Self {
        self.additional_ca_paths = paths.iter().map(AsRef::as_ref).map(PathBuf::from).collect();
        self
    }
}
