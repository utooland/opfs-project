//! Lazy package manager - downloads tgz only, extracts on-demand

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::fuse;
use crate::pack;

/// Public package paths for lazy extraction
pub struct PublicPackagePaths {
    pub tgz_store_path: PathBuf,
}

impl PublicPackagePaths {
    pub fn new(name: &str, tgz_url: &str) -> Self {
        let url_path: Vec<_> = tgz_url.split('/').collect();
        let tgz_file_name = url_path.last().unwrap_or(&"package.tgz");
        Self {
            tgz_store_path: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}")),
        }
    }
}

/// Check if tgz file is cached
pub async fn is_tgz_cached(paths: &PublicPackagePaths) -> bool {
    tokio_fs_ext::metadata(&paths.tgz_store_path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Download tgz only (no extraction)
pub async fn download_only(
    name: &str,
    version: &str,
    tgz_url: &str,
    integrity: Option<&str>,
    shasum: Option<&str>,
) -> Result<Vec<u8>> {
    let paths = PublicPackagePaths::new(name, tgz_url);

    // Check if tgz already exists with valid integrity
    if let Ok(existing_bytes) = tokio_fs_ext::read(&paths.tgz_store_path).await {
        if integrity.is_some() || shasum.is_some() {
            if pack::verify_integrity(&existing_bytes, integrity, shasum) {
                return Ok(existing_bytes);
            }
        }
    }

    // Download new file
    let bytes = download_bytes(tgz_url).await?;

    // Verify integrity
    if integrity.is_some() || shasum.is_some() {
        if !pack::verify_integrity(&bytes, integrity, shasum) {
            return Err(anyhow::anyhow!(
                "{}@{}: integrity check failed",
                name,
                version
            ));
        }
    }

    // Save tgz to store
    save_tgz(&paths.tgz_store_path, &bytes).await?;

    Ok(bytes)
}

/// Create lazy fuse links pointing to tgz file
pub async fn create_fuse_links_lazy(
    tgz_path: &PathBuf,
    path_keys: &[String],
    prefix: Option<&str>,
) -> Result<()> {
    for path_key in path_keys {
        let target = PathBuf::from(path_key);
        fuse::fuse_link_with_prefix(tgz_path, &target, prefix)
            .await
            .context(format!("failed to create lazy fuse link for {}", path_key))?;
    }
    Ok(())
}

/// Write bytes to file
async fn save_tgz(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    if let Some(parent_dir) = path.parent() {
        tokio_fs_ext::create_dir_all(parent_dir).await?;
    }
    tokio_fs_ext::write(path, bytes).await?;
    Ok(())
}

/// Download bytes from URL
async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .context(format!("Failed to download from {}", url))?;

    let bytes = response
        .bytes()
        .await
        .context(format!("Failed to read response from {}", url))?;

    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use crate::test_utils;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    async fn test_public_package_paths_new() {
        let paths = PublicPackagePaths::new(
            "lodash",
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
        );
        assert_eq!(
            paths.tgz_store_path.to_string_lossy(),
            "/stores/lodash/-/lodash-4.17.21.tgz"
        );
    }

    #[wasm_bindgen_test]
    async fn test_public_package_paths_scoped() {
        let paths = PublicPackagePaths::new(
            "@types/node",
            "https://registry.npmjs.org/@types/node/-/node-18.0.0.tgz",
        );
        assert_eq!(
            paths.tgz_store_path.to_string_lossy(),
            "/stores/@types/node/-/node-18.0.0.tgz"
        );
    }

    #[wasm_bindgen_test]
    async fn test_is_tgz_cached_not_exists() {
        let paths = PublicPackagePaths::new(
            "nonexistent-package",
            "https://registry.npmjs.org/nonexistent/-/nonexistent-1.0.0.tgz",
        );
        assert!(!is_tgz_cached(&paths).await);
    }

    #[wasm_bindgen_test]
    async fn test_download_only_and_cache() {
        test_utils::init_tracing();

        let name = "lodash";
        let version = "4.17.21";
        let url = "https://registry.npmmirror.com/lodash/-/lodash-4.17.21.tgz";

        // First download
        let result = download_only(name, version, url, None, None).await;

        if let Ok(bytes) = result {
            assert!(bytes.len() > 1000, "tgz should be larger than 1000 bytes");

            // Verify cached
            let paths = PublicPackagePaths::new(name, url);
            assert!(is_tgz_cached(&paths).await);

            // Second download should use cache
            let cached_result = download_only(name, version, url, None, None).await;
            assert!(cached_result.is_ok());
            assert_eq!(cached_result.unwrap().len(), bytes.len());
        }
    }

    #[wasm_bindgen_test]
    async fn test_download_only_with_integrity() {
        test_utils::init_tracing();

        let name = "lodash";
        let version = "4.17.21";
        let url = "https://registry.npmmirror.com/lodash/-/lodash-4.17.21.tgz";
        let integrity = "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==";

        let result = download_only(name, version, url, Some(integrity), None).await;

        // Should succeed with correct integrity
        if result.is_ok() {
            let bytes = result.unwrap();
            assert!(bytes.len() > 1000);
        }
    }

    #[wasm_bindgen_test]
    async fn test_download_only_invalid_integrity() {
        test_utils::init_tracing();

        // Clean up any cached file first
        let paths = PublicPackagePaths::new(
            "test-invalid-integrity",
            "https://registry.npmmirror.com/lodash/-/lodash-4.17.21.tgz",
        );
        let _ = tokio_fs_ext::remove_file(&paths.tgz_store_path).await;

        let result = download_only(
            "test-invalid-integrity",
            "4.17.21",
            "https://registry.npmmirror.com/lodash/-/lodash-4.17.21.tgz",
            Some("sha512-invalid"),
            None,
        )
        .await;

        // Should fail with invalid integrity
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("integrity check failed"));
        }
    }

    #[wasm_bindgen_test]
    async fn test_create_fuse_links_lazy() {
        test_utils::init_tracing();

        let tgz_path = PathBuf::from("/stores/test-pkg/-/test-pkg-1.0.0.tgz");
        let path_keys = vec![
            "node_modules/test-pkg".to_string(),
            "node_modules/other/node_modules/test-pkg".to_string(),
        ];

        let result = create_fuse_links_lazy(&tgz_path, &path_keys, Some("package")).await;
        assert!(result.is_ok());

        // Verify fuse links were created
        for path_key in &path_keys {
            let fuse_link_path = format!("{}/fuse.link", path_key);
            let content = tokio_fs_ext::read_to_string(&fuse_link_path).await;
            assert!(content.is_ok());
            let content = content.unwrap();
            assert!(content.contains("/stores/test-pkg/-/test-pkg-1.0.0.tgz"));
            assert!(content.contains("|package"));
        }
    }
}
