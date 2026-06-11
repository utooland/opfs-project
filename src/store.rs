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
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    }

    /// Ensure a tgz is on disk — download if missing.
    ///
    /// When integrity metadata is available, cached files are re-verified
    /// before being trusted. Without integrity metadata, non-empty cached
    /// files use an O(1) metadata fast path.
    ///
    /// Returns `was_fresh` — `true` when the tgz was freshly downloaded
    /// (i.e. it was missing or an empty placeholder). Callers should use this
    /// flag to invalidate stale extraction caches.
    pub async fn ensure_tgz(
        &self,
        name: &str,
        version: &str,
        tgz_url: &str,
        integrity: Option<&str>,
        shasum: Option<&str>,
    ) -> Result<bool, OpfsError> {
        let store_path = self.tgz_path(name, tgz_url);

        if let Ok(metadata) = tokio_fs_ext::metadata(&store_path).await
            && metadata.is_file()
        {
            if metadata.len() == 0 {
                tracing::warn!("{name}@{version}: cached tgz is empty, re-downloading");
            } else if integrity.is_none() && shasum.is_none() {
                return Ok(false);
            } else if let Ok(existing) = tokio_fs_ext::read(&store_path).await {
                match archive::verify_integrity(&existing, integrity, shasum) {
                    VerifyResult::Verified | VerifyResult::NoHashAvailable => {
                        return Ok(false);
                    }
                    VerifyResult::Failed => {
                        tracing::warn!(
                            "{name}@{version}: cached tgz failed integrity, re-downloading"
                        );
                    }
                }
            }
        }

        // Not cached — download, verify, and persist.
        let bytes = self.download_with_retry(tgz_url).await?;

        if archive::verify_integrity(&bytes, integrity, shasum).is_failed() {
            return Err(OpfsError::IntegrityFailed {
                package: name.to_string(),
                version: version.to_string(),
            });
        }

        self.save(&store_path, &bytes).await?;
        Ok(true)
    }

    /// Fetch a tgz — returns cached bytes if valid, otherwise downloads.
    ///
    /// Performs full integrity re-verification on cached files. Use
    /// [`ensure_tgz`] when you only need the file on disk (e.g. install flow).
    ///
    /// Returns `(bytes, was_fresh)` where `was_fresh` is `true` when the tgz
    /// was re-downloaded (e.g. because the cached copy failed integrity).
    pub async fn fetch_tgz(
        &self,
        name: &str,
        version: &str,
        tgz_url: &str,
        integrity: Option<&str>,
        shasum: Option<&str>,
    ) -> Result<(Bytes, bool), OpfsError> {
        let store_path = self.tgz_path(name, tgz_url);

        // Try cached file — full read + integrity verification
        if let Ok(existing) = tokio_fs_ext::read(&store_path).await {
            if existing.is_empty() {
                tracing::warn!("{name}@{version}: cached tgz is empty, re-downloading");
            } else {
                match archive::verify_integrity(&existing, integrity, shasum) {
                    VerifyResult::Verified | VerifyResult::NoHashAvailable => {
                        return Ok((Bytes::from(existing), false));
                    }
                    VerifyResult::Failed => {
                        tracing::warn!(
                            "{name}@{version}: cached tgz failed integrity, re-downloading"
                        );
                    }
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
        Ok((Bytes::from(bytes), true))
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
                let delay = self
                    .retry_base_delay_ms
                    .saturating_mul(1u64 << (attempt - 1).min(63));
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
        let bytes = resp.bytes().await?.to_vec();
        if bytes.is_empty() {
            return Err(OpfsError::Other(format!("empty response body for {url}")));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);

    use std::io::ErrorKind;
    use std::path::Path;

    use wasm_bindgen_test::*;

    use super::Store;
    use crate::config::Config;
    use crate::error::OpfsError;

    async fn remove_dir_all_if_exists(path: &Path) {
        match tokio_fs_ext::remove_dir_all(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", path.display()),
        }
    }

    #[wasm_bindgen_test]
    async fn test_zero_byte_tgz_is_not_cached() {
        let base = Path::new("/test_zero_byte_tgz_is_not_cached");
        remove_dir_all_if_exists(base).await;

        let store = Store::new(&Config {
            store_root: base.join("stores"),
            ..Config::default()
        });
        let tgz_url = "http://127.0.0.1:65535/pkg/-/pkg-1.0.0.tgz";
        let tgz_path = store.tgz_path("pkg", tgz_url);

        tokio_fs_ext::create_dir_all(tgz_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&tgz_path, b"").await.unwrap();

        assert!(!store.is_cached("pkg", tgz_url).await);

        remove_dir_all_if_exists(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_ensure_tgz_redownloads_zero_byte_cache() {
        let base = Path::new("/test_ensure_tgz_redownloads_zero_byte_cache");
        remove_dir_all_if_exists(base).await;

        let store = Store::new(&Config {
            store_root: base.join("stores"),
            download_retries: 1,
            retry_base_delay_ms: 1,
            ..Config::default()
        });
        let tgz_url = "http://127.0.0.1:65535/pkg/-/pkg-1.0.0.tgz";
        let tgz_path = store.tgz_path("pkg", tgz_url);

        tokio_fs_ext::create_dir_all(tgz_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&tgz_path, b"").await.unwrap();

        let result = store
            .ensure_tgz(
                "pkg",
                "1.0.0",
                tgz_url,
                Some("sha512-not-the-empty-file"),
                None,
            )
            .await;

        assert!(matches!(result, Err(OpfsError::Network(_))));
        assert_eq!(tokio_fs_ext::metadata(&tgz_path).await.unwrap().len(), 0);

        remove_dir_all_if_exists(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_ensure_tgz_redownloads_failed_integrity_cache() {
        let base = Path::new("/test_ensure_tgz_redownloads_failed_integrity");
        remove_dir_all_if_exists(base).await;

        let store = Store::new(&Config {
            store_root: base.join("stores"),
            download_retries: 1,
            retry_base_delay_ms: 1,
            ..Config::default()
        });
        let tgz_url = "http://127.0.0.1:65535/pkg/-/pkg-1.0.0.tgz";
        let tgz_path = store.tgz_path("pkg", tgz_url);

        tokio_fs_ext::create_dir_all(tgz_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&tgz_path, b"corrupt cached tgz")
            .await
            .unwrap();

        let result = store
            .ensure_tgz(
                "pkg",
                "1.0.0",
                tgz_url,
                Some("sha512-not-the-corrupt-cache"),
                None,
            )
            .await;

        assert!(matches!(result, Err(OpfsError::Network(_))));

        remove_dir_all_if_exists(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_fetch_tgz_redownloads_zero_byte_cache_without_integrity() {
        let base = Path::new("/test_fetch_tgz_redownloads_zero_byte_cache");
        remove_dir_all_if_exists(base).await;

        let store = Store::new(&Config {
            store_root: base.join("stores"),
            download_retries: 1,
            retry_base_delay_ms: 1,
            ..Config::default()
        });
        let tgz_url = "http://127.0.0.1:65535/pkg/-/pkg-1.0.0.tgz";
        let tgz_path = store.tgz_path("pkg", tgz_url);

        tokio_fs_ext::create_dir_all(tgz_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&tgz_path, b"").await.unwrap();

        let result = store.fetch_tgz("pkg", "1.0.0", tgz_url, None, None).await;

        assert!(matches!(result, Err(OpfsError::Network(_))));
        assert_eq!(tokio_fs_ext::metadata(&tgz_path).await.unwrap().len(), 0);

        remove_dir_all_if_exists(base).await;
    }
}
