//! Tgz store — download, verify integrity, and persist to OPFS.

use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::archive;
use crate::config::Config;
use crate::error::{OpfsError, VerifyResult};

/// Manages the tgz file store on OPFS.
pub struct Store {
    root: PathBuf,
    retries: u32,
    retry_base_delay_ms: u64,
}

impl Store {
    pub fn new(config: &Config) -> Self {
        Self {
            root: config.store_root.clone(),
            retries: config.download_retries,
            retry_base_delay_ms: config.retry_base_delay_ms,
        }
    }

    /// Compute the OPFS path where a package tgz is stored.
    pub fn tgz_path(&self, name: &str, tgz_url: &str) -> PathBuf {
        let file_name = tgz_url.rsplit('/').next().unwrap_or("package.tgz");
        self.root.join(name).join("-").join(file_name)
    }

    /// Check whether the tgz for a package is already on disk.
    pub async fn is_cached(&self, name: &str, tgz_url: &str) -> bool {
        let path = self.tgz_path(name, tgz_url);
        tokio_fs_ext::metadata(&path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// Fetch a tgz — returns cached bytes if valid, otherwise downloads.
    pub async fn fetch_tgz(
        &self,
        name: &str,
        version: &str,
        tgz_url: &str,
        integrity: Option<&str>,
        shasum: Option<&str>,
    ) -> Result<Bytes, OpfsError> {
        let store_path = self.tgz_path(name, tgz_url);

        // Try cached file
        if let Ok(existing) = tokio_fs_ext::read(&store_path).await {
            match archive::verify_integrity(&existing, integrity, shasum) {
                VerifyResult::Verified | VerifyResult::NoHashAvailable => {
                    return Ok(Bytes::from(existing));
                }
                VerifyResult::Failed => {
                    tracing::warn!("{name}@{version}: cached tgz failed integrity, re-downloading");
                }
            }
        }

        // Download with retry
        let bytes = self.download_with_retry(tgz_url).await?;

        // Verify downloaded bytes
        if archive::verify_integrity(&bytes, integrity, shasum).is_failed() {
            return Err(OpfsError::IntegrityFailed {
                package: name.to_string(),
                version: version.to_string(),
            });
        }

        // Persist
        self.save(&store_path, &bytes).await?;
        Ok(Bytes::from(bytes))
    }

    // ── private ──────────────────────────────────────────────────────

    async fn save(&self, path: &Path, bytes: &[u8]) -> Result<(), OpfsError> {
        if let Some(parent) = path.parent() {
            tokio_fs_ext::create_dir_all(parent).await?;
        }
        tokio_fs_ext::write(path, bytes).await?;
        Ok(())
    }

    async fn download_with_retry(&self, url: &str) -> Result<Vec<u8>, OpfsError> {
        let mut last_err = None;
        for attempt in 0..self.retries {
            if attempt > 0 {
                let delay = self.retry_base_delay_ms.saturating_mul(1u64 << (attempt - 1).min(63));
                wasmtimer::tokio::sleep(std::time::Duration::from_millis(delay)).await;
            }
            match self.download_once(url).await {
                Ok(b) => return Ok(b),
                Err(e) => {
                    tracing::warn!(
                        "download {}/{} for {url} failed: {e}",
                        attempt + 1,
                        self.retries
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| OpfsError::Other(format!("download failed: {url}"))))
    }

    async fn download_once(&self, url: &str) -> Result<Vec<u8>, OpfsError> {
        let resp = reqwest::get(url).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OpfsError::Http {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }
}
