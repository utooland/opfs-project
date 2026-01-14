//! Lazy package manager - downloads tgz only, extracts on-demand

use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::fuse;
use crate::pack;
use crate::package_lock::{LockPackage, PackageLock};

/// Types of dependencies that can be omitted during install
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmitType {
    /// Skip dev dependencies (packages with dev: true)
    Dev,
    /// Skip optional dependencies (packages with optional: true)
    Optional,
}

/// Options for install
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Maximum concurrent downloads (default: 20)
    pub max_concurrent_downloads: Option<usize>,
    /// Types of dependencies to skip
    pub omit: Vec<OmitType>,
}

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

const DEFAULT_MAX_CONCURRENT_DOWNLOADS: usize = 20;

/// Internal package group structure for deduplication
struct PackageGroup {
    name: String,
    version: String,
    tgz_url: String,
    integrity: Option<String>,
    shasum: Option<String>,
    target_paths: Vec<String>,
}

/// Check if a package should be omitted based on omit flags
fn should_omit(pkg: &LockPackage, omit: &[OmitType]) -> bool {
    omit.iter().any(|o| match o {
        OmitType::Dev => pkg.dev == Some(true),
        OmitType::Optional => pkg.optional == Some(true),
    })
}

/// Install dependencies from package-lock.json
/// Downloads tgz files only, creates fuse links for lazy extraction
///
/// # Arguments
/// * `lock` - The parsed package-lock.json
/// * `options` - Install options (use `None` or `Some(InstallOptions::default())` for defaults)
///
/// # Example
/// ```ignore
/// // Default options
/// install(&lock, None).await?;
///
/// // With options
/// install(&lock, Some(InstallOptions {
///     omit: vec![OmitType::Dev],
///     ..Default::default()
/// })).await?;
/// ```
pub async fn install(lock: &PackageLock, options: Option<InstallOptions>) -> Result<()> {
    let opts = options.unwrap_or_default();
    let omit = &opts.omit;

    // Step 1: Group packages by tgz URL to deduplicate downloads
    let mut groups: HashMap<String, PackageGroup> = HashMap::new();

    for (path, pkg) in lock.packages.iter().filter(|(path, _)| !path.is_empty()) {
        // Skip packages based on omit options
        if should_omit(pkg, omit) {
            let name = pkg.get_name(path);
            let version = pkg.get_version();
            let reason = if pkg.dev == Some(true) { "dev" } else { "optional" };
            tracing::debug!("{}@{}: skipped ({})", name, version, reason);
            continue;
        }

        // Skip optional packages with platform constraints (os/cpu)
        // These are platform-specific binaries that won't work in WASM environment
        if pkg.optional == Some(true) && (pkg.os.is_some() || pkg.cpu.is_some()) {
            let name = pkg.get_name(path);
            let version = pkg.get_version();
            tracing::debug!("{}@{}: skipped (optional with platform constraints)", name, version);
            continue;
        }

        let name = pkg.get_name(path);
        let version = pkg.get_version();
        let tgz_url = match &pkg.resolved {
            Some(u) => u.clone(),
            None => {
                tracing::warn!("{}@{}: no resolved field, skipping", name, version);
                continue;
            }
        };

        groups
            .entry(tgz_url.clone())
            .or_insert_with(|| PackageGroup {
                name,
                version,
                tgz_url,
                integrity: pkg.integrity.clone(),
                shasum: pkg.shasum.clone(),
                target_paths: Vec::new(),
            })
            .target_paths
            .push(path.clone());
    }

    // Step 2: Partition by cache status (check if tgz already downloaded)
    let mut cached: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut to_download: Vec<PackageGroup> = Vec::new();

    for group in groups.into_values() {
        let paths = PublicPackagePaths::new(&group.name, &group.tgz_url);
        if is_tgz_cached(&paths).await {
            cached.push((paths.tgz_store_path, group.target_paths));
        } else {
            to_download.push(group);
        }
    }

    // Step 3: Create lazy fuse links for cached packages
    for (tgz_path, target_paths) in cached {
        create_fuse_links_lazy(&tgz_path, &target_paths, Some("package")).await?;
    }

    // Step 4: Download non-cached packages
    if !to_download.is_empty() {
        let max_concurrent = opts.max_concurrent_downloads.unwrap_or(DEFAULT_MAX_CONCURRENT_DOWNLOADS);

        let download_results: Vec<_> = stream::iter(to_download.into_iter().map(|group| {
            async move {
                let _bytes = download_only(
                    &group.name,
                    &group.version,
                    &group.tgz_url,
                    group.integrity.as_deref(),
                    group.shasum.as_deref(),
                )
                .await?;
                Ok::<_, anyhow::Error>((group.name, group.tgz_url, group.target_paths))
            }
        }))
        .buffer_unordered(max_concurrent)
        .collect()
        .await;

        // Create lazy fuse links for downloaded packages
        for result in download_results {
            let (name, tgz_url, target_paths) = result?;
            let paths = PublicPackagePaths::new(&name, &tgz_url);
            create_fuse_links_lazy(&paths.tgz_store_path, &target_paths, Some("package"))
                .await?;
        }
    }

    Ok(())
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

    #[wasm_bindgen_test]
    async fn test_install_empty_packages() {
        test_utils::init_tracing();

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages: HashMap::new(),
            dependencies: None,
        };

        let result = install(&lock, None).await;
        assert!(result.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_install_skip_no_resolved() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        let mut packages = HashMap::new();
        // Root package (should be skipped)
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });
        // Package without resolved (should be skipped with warning)
        packages.insert("node_modules/no-resolved-pkg".to_string(), LockPackage {
            name: Some("no-resolved-pkg".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        // Should succeed, just skip the package without resolved
        let result = install(&lock, None).await;
        assert!(result.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_install_single_package() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        // Clean up first
        let _ = tokio_fs_ext::remove_dir_all("node_modules/is-number").await;

        let mut packages = HashMap::new();
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });
        packages.insert("node_modules/is-number".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        let result = install(&lock, Some(InstallOptions {
            max_concurrent_downloads: Some(5),
            omit: vec![],
        })).await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());

        // Verify fuse link was created
        let fuse_link = tokio_fs_ext::read_to_string("node_modules/is-number/fuse.link").await;
        assert!(fuse_link.is_ok());
        assert!(fuse_link.unwrap().contains("is-number-7.0.0.tgz"));
    }

    #[wasm_bindgen_test]
    async fn test_install_deduplication() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        // Clean up first
        let _ = tokio_fs_ext::remove_dir_all("node_modules/is-number").await;
        let _ = tokio_fs_ext::remove_dir_all("node_modules/other/node_modules/is-number").await;

        let mut packages = HashMap::new();
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });
        // Same package at two locations (should deduplicate download)
        packages.insert("node_modules/is-number".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });
        packages.insert("node_modules/other/node_modules/is-number".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        let result = install(&lock, None).await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());

        // Verify both fuse links were created
        let fuse_link1 = tokio_fs_ext::read_to_string("node_modules/is-number/fuse.link").await;
        assert!(fuse_link1.is_ok());

        let fuse_link2 = tokio_fs_ext::read_to_string("node_modules/other/node_modules/is-number/fuse.link").await;
        assert!(fuse_link2.is_ok());

        // Both should point to the same tgz
        assert_eq!(fuse_link1.unwrap(), fuse_link2.unwrap());
    }

    #[wasm_bindgen_test]
    async fn test_install_omit_dev() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        // Clean up first
        let _ = tokio_fs_ext::remove_dir_all("node_modules/prod-pkg").await;
        let _ = tokio_fs_ext::remove_dir_all("node_modules/dev-pkg").await;

        let mut packages = HashMap::new();
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Production dependency
        packages.insert("node_modules/prod-pkg".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,  // Not a dev dependency
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Dev dependency (should be skipped)
        packages.insert("node_modules/dev-pkg".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: Some(true),  // This is a dev dependency
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        // Install with omit dev
        let result = install(&lock, Some(InstallOptions {
            max_concurrent_downloads: None,
            omit: vec![OmitType::Dev],
        })).await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());

        // Verify prod-pkg was installed
        let prod_link = tokio_fs_ext::read_to_string("node_modules/prod-pkg/fuse.link").await;
        assert!(prod_link.is_ok(), "prod-pkg should be installed");

        // Verify dev-pkg was NOT installed
        let dev_link = tokio_fs_ext::read_to_string("node_modules/dev-pkg/fuse.link").await;
        assert!(dev_link.is_err(), "dev-pkg should be skipped");
    }

    #[wasm_bindgen_test]
    async fn test_install_omit_optional() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        // Clean up first
        let _ = tokio_fs_ext::remove_dir_all("node_modules/required-pkg").await;
        let _ = tokio_fs_ext::remove_dir_all("node_modules/optional-pkg").await;

        let mut packages = HashMap::new();
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Required dependency
        packages.insert("node_modules/required-pkg".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,  // Not optional
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Optional dependency (should be skipped)
        packages.insert("node_modules/optional-pkg".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: Some(true),  // This is optional
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        // Install with omit optional
        let result = install(&lock, Some(InstallOptions {
            max_concurrent_downloads: None,
            omit: vec![OmitType::Optional],
        })).await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());

        // Verify required-pkg was installed
        let required_link = tokio_fs_ext::read_to_string("node_modules/required-pkg/fuse.link").await;
        assert!(required_link.is_ok(), "required-pkg should be installed");

        // Verify optional-pkg was NOT installed
        let optional_link = tokio_fs_ext::read_to_string("node_modules/optional-pkg/fuse.link").await;
        assert!(optional_link.is_err(), "optional-pkg should be skipped");
    }

    #[wasm_bindgen_test]
    async fn test_skip_optional_with_platform_constraints() {
        test_utils::init_tracing();

        use crate::package_lock::LockPackage;

        // Clean up first
        let _ = tokio_fs_ext::remove_dir_all("node_modules/normal-pkg").await;
        let _ = tokio_fs_ext::remove_dir_all("node_modules/platform-optional-pkg").await;

        let mut packages = HashMap::new();
        packages.insert("".to_string(), LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Normal dependency (should be installed)
        packages.insert("node_modules/normal-pkg".to_string(), LockPackage {
            name: Some("is-number".to_string()),
            version: Some("7.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: None,
            has_install_script: None,
            workspaces: None,
            os: None,
            cpu: None,
        });

        // Optional dependency with platform constraints (should be skipped even without --omit optional)
        packages.insert("node_modules/platform-optional-pkg".to_string(), LockPackage {
            name: Some("esbuild-darwin-arm64".to_string()),
            version: Some("0.15.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/is-number/-/is-number-7.0.0.tgz".to_string()),
            integrity: Some("sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==".to_string()),
            shasum: None,
            license: None,
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            requires: None,
            bin: None,
            peer: None,
            dev: None,
            optional: Some(true),
            has_install_script: None,
            workspaces: None,
            os: Some(serde_json::json!(["darwin"])),
            cpu: Some(serde_json::json!(["arm64"])),
        });

        let lock = PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 3,
            requires: true,
            packages,
            dependencies: None,
        };

        // Install WITHOUT omit optional - platform-specific optional should still be skipped
        let result = install(&lock, None).await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());

        // Verify normal-pkg was installed
        let normal_link = tokio_fs_ext::read_to_string("node_modules/normal-pkg/fuse.link").await;
        assert!(normal_link.is_ok(), "normal-pkg should be installed");

        // Verify platform-optional-pkg was NOT installed (skipped due to os/cpu constraints)
        let platform_link = tokio_fs_ext::read_to_string("node_modules/platform-optional-pkg/fuse.link").await;
        assert!(platform_link.is_err(), "platform-optional-pkg should be skipped");
    }
}
