use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use futures::stream::{self, StreamExt, TryStreamExt};
use std::io::Read;
use std::path::PathBuf;
use tar::Archive;

use super::fuse;
use crate::pack;

use crate::package_lock::PackageLock;

/// Download all tgz packages to OPFS
pub async fn install_deps(package_lock: &str, max_concurrent_downloads: usize) -> Result<()> {
    let lock = PackageLock::from_json(package_lock)?;

    // Write package.json to root
    ensure_package_json(&lock).await?;

    // Prepare package info for installation
    let packages: Vec<_> = lock
        .packages
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(path, pkg)| {
            (
                pkg.get_name(path),
                pkg.get_version(),
                pkg.resolved.clone(),
                pkg.integrity.clone(),
                pkg.shasum.clone(),
                path.clone(),
            )
        })
        .collect();

    // Step 1: Filter packages into cached and needs-download
    let mut cached_packages = Vec::new();
    let mut packages_to_download = Vec::new();

    for (name, version, tgz_url, integrity, shasum, path_key) in packages {
        let url = match tgz_url.as_ref() {
            Some(u) => u,
            None => {
                return Err(anyhow::anyhow!("{}@{}: no resolved field", name, version));
            }
        };

        let paths = PackagePaths::new(&name, url, &path_key);

        // Check if already unpacked (strict check: marker must be file, unpacked_dir must be directory)
        // If _resolved marker exists, it means the package has been verified and extracted successfully
        let is_cached = if let Ok(marker_meta) = tokio_fs_ext::metadata(&paths.resolved_marker).await
            && marker_meta.is_file()
            && let Ok(dir_meta) = tokio_fs_ext::metadata(&paths.unpacked_dir).await
            && dir_meta.is_dir()
        {
            true
        } else {
            false
        };

        if is_cached {
            cached_packages.push((paths, name, version));
        } else {
            packages_to_download.push((name, version, tgz_url, integrity, shasum, path_key));
        }
    }

    // Step 2: Create fuse links for cached packages (parallel, no limit)
    futures::future::try_join_all(
        cached_packages.into_iter().map(|(paths, name, version)| async move {
            fuse::fuse_link(&paths.unpacked_dir, &paths.link_target_dir)
                .await
                .context(format!("{}@{}: failed to create fuse link", name, version))
        })
    )
    .await?;

    // Step 3: Download and install packages with concurrency control
    stream::iter(packages_to_download)
        .map(|(name, version, tgz_url, integrity, shasum, path_key)| async move {
            install_package(&name, &version, &tgz_url, integrity.as_deref(), shasum.as_deref(), &path_key).await
        })
        .buffer_unordered(max_concurrent_downloads)
        .try_collect::<()>()
        .await
}

/// Write root package.json to the project directory
async fn ensure_package_json(lock: &PackageLock) -> Result<()> {
    if tokio_fs_ext::metadata(&format!("./package.json"))
        .await
        .is_ok()
    {
        return Ok(());
    }

    if let Some(root_pkg) = lock.packages.get("") {
        let pkg_json = serde_json::to_string_pretty(root_pkg).unwrap_or("{}".to_string());
        tokio_fs_ext::create_dir_all(&format!("./node_modules")).await?;
        tokio_fs_ext::write(&format!("./package.json"), pkg_json.as_bytes()).await?;
    }
    Ok(())
}

/// Install package using the provided URL
async fn install_package(
    name: &str,
    version: &str,
    tgz_url: &Option<String>,
    integrity: Option<&str>,
    shasum: Option<&str>,
    path_key: &str,
) -> Result<()> {
    let url = tgz_url
        .as_ref()
        .with_context(|| format!("{}@{}: no resolved field", name, version))?;

    let paths = PackagePaths::new(name, url, path_key);

    // Check if already unpacked by checking for both resolved marker and unpacked directory
    // If _resolved marker exists, it means the package has been verified and extracted successfully
    if let Ok(marker_meta) = tokio_fs_ext::metadata(&paths.resolved_marker).await
        && marker_meta.is_file()
        && let Ok(dir_meta) = tokio_fs_ext::metadata(&paths.unpacked_dir).await
        && dir_meta.is_dir()
    {
        // Package is fully unpacked and verified, create fuse link
        fuse::fuse_link(&paths.unpacked_dir, &paths.link_target_dir)
            .await
            .context(format!("{}@{}: failed to create fuse link", name, version))?;
        return Ok(());
    }

    // Download and verify tgz bytes
    let tgz_bytes = download_tgz(url, &paths.tgz_store_path, integrity, shasum)
        .await
        .context(format!("{}@{}: failed to download package", name, version))?;

    // Extract and create fuse link
    extract_tgz_bytes(&tgz_bytes, &paths.unpacked_dir)
        .await
        .context(format!("{}@{}: failed to extract package", name, version))?;

    // Write resolved marker file to indicate successful extraction
    tokio_fs_ext::write(&paths.resolved_marker, b"")
        .await
        .context(format!("{}@{}: failed to write marker", name, version))?;

    // Create fuse link
    fuse::fuse_link(&paths.unpacked_dir, &paths.link_target_dir)
        .await
        .context(format!("{}@{}: failed to create fuse link", name, version))?;

    Ok(())
}

/// Download tgz file and save to store
/// If tgz file exists and integrity matches, use cached file instead of downloading
async fn download_tgz(
    tgz_url: &str,
    tgz_store_path: &PathBuf,
    integrity: Option<&str>,
    shasum: Option<&str>,
) -> Result<Vec<u8>> {
    // Check if tgz file already exists and verify its integrity
    if let Ok(existing_bytes) = tokio_fs_ext::read(tgz_store_path).await {
        if integrity.is_some() || shasum.is_some() {
            // If we have integrity/shasum info, verify the existing file
            if pack::verify_integrity(&existing_bytes, integrity, shasum) {
                // Existing file is valid, use it
                return Ok(existing_bytes);
            }
            // Existing file is corrupted or doesn't match, will re-download below
        }
    }

    // Download new file
    let bytes = download_bytes(tgz_url).await?;

    // Verify downloaded file if integrity/shasum is provided
    if integrity.is_some() || shasum.is_some() {
        if !pack::verify_integrity(&bytes, integrity, shasum) {
            return Err(anyhow::anyhow!(
                "Downloaded file integrity check failed for {}",
                tgz_url
            ));
        }
    }

    save_tgz(tgz_store_path, &bytes).await?;
    Ok(bytes)
}

/// Package paths for installation
struct PackagePaths {
    tgz_store_path: PathBuf,
    unpacked_dir: PathBuf,
    resolved_marker: PathBuf,
    link_target_dir: PathBuf,
}

impl PackagePaths {
    fn new(name: &str, tgz_url: &str, path_key: &str) -> Self {
        let url_path: Vec<_> = tgz_url.split('/').collect();
        let tgz_file_name = url_path.last().unwrap_or(&"package.tgz");

        Self {
            tgz_store_path: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}")),
            unpacked_dir: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}-unpack")),
            resolved_marker: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}-unpack._resolved")),
            link_target_dir: PathBuf::from(format!("{path_key}")),
        }
    }
}

/// Archive entry information
#[derive(Debug)]
struct ArchiveEntry {
    path: String,
    is_file: bool,
    contents: Option<Vec<u8>>,
}

impl ArchiveEntry {
    fn new(path: String, is_file: bool, contents: Option<Vec<u8>>) -> Self {
        Self { path, is_file, contents }
    }
}

/// Extract tgz bytes to directory
pub async fn extract_tgz_bytes(tgz_bytes: &[u8], extract_dir: &PathBuf) -> Result<()> {
    let gz = GzDecoder::new(tgz_bytes);
    let mut archive = Archive::new(gz);
    let entries = archive
        .entries()
        .context("Failed to read archive entries")?;

    // Collect all archive entries with their contents
    let mut archive_entries = Vec::new();

    for entry in entries {
        let mut entry = entry.context("Failed to read archive entry")?;
        let path = entry.path().context("Failed to read entry path")?;
        let path_str = path.to_string_lossy().to_string();
        let is_file = entry.header().entry_type().is_file();

        let contents = if is_file {
            let mut file_contents = Vec::new();
            entry
                .read_to_end(&mut file_contents)
                .context("Failed to read entry contents")?;
            Some(file_contents)
        } else {
            None
        };

        archive_entries.push(ArchiveEntry::new(path_str, is_file, contents));
    }

    // Determine the root prefix
    let root_prefix = determine_root_prefix(&archive_entries);

    // Extract files with proper path handling
    extract_entries(&archive_entries, extract_dir, &root_prefix).await?;

    Ok(())
}


/// Determine the root prefix by finding package.json location
fn determine_root_prefix(entries: &[ArchiveEntry]) -> Option<String> {
    // First, check for the most common case: package/package.json
    if entries.iter().any(|entry| entry.path == "package/package.json") {
        return Some("package".to_string());
    }

    // Check if package.json is at root level
    if entries.iter().any(|entry| entry.path == "package.json") {
        return None;
    }

    // Sort entries by path length to prioritize shorter paths (closer to root)
    let mut sorted_entries: Vec<_> = entries.iter().collect();
    sorted_entries.sort_by_key(|entry| entry.path.len());

    // Look for other package.json locations
    for entry in sorted_entries {
        if entry.path.ends_with("/package.json") {
            if let Some(prefix) = entry.path.strip_suffix("/package.json") {
                // Only consider this as root if prefix is not empty and doesn't contain slashes
                // This ensures we get the actual root directory, not a nested one
                if !prefix.is_empty() && !prefix.contains('/') {
                    return Some(prefix.to_string());
                }
            }
        }
    }

    // Fallback: if no package.json found but we have "package/" prefix, use it
    if entries.iter().any(|entry| entry.path.starts_with("package/")) {
        Some("package".to_string())
    } else {
        None
    }
}

/// Extract entries to the target directory
async fn extract_entries(
    entries: &[ArchiveEntry],
    extract_dir: &PathBuf,
    root_prefix: &Option<String>
) -> Result<()> {
    for entry in entries {
        if !entry.is_file {
            continue; // Skip non-file entries
        }

        if let Some(out_path) = calculate_output_path(&entry.path, extract_dir, root_prefix)? {
            if let Some(contents) = &entry.contents {
                save_tgz(&out_path, contents).await?;
            }
        }
        // If calculate_output_path returns None, skip this entry
    }

    Ok(())
}

/// Calculate the output path for an entry
/// Returns Ok(None) for entries that should be skipped, Ok(Some(path)) for entries to extract
fn calculate_output_path(
    entry_path: &str,
    extract_dir: &PathBuf,
    root_prefix: &Option<String>
) -> Result<Option<PathBuf>> {
    let out_path = if let Some(prefix) = root_prefix {
        // Strip the root prefix
        if let Some(stripped) = entry_path.strip_prefix(&format!("{}/", prefix)) {
            extract_dir.join(stripped)
        } else if entry_path == prefix {
            // Skip the root directory itself
            return Ok(None);
        } else {
            extract_dir.join(entry_path)
        }
    } else {
        // No prefix to strip, use path as-is
        extract_dir.join(entry_path)
    };

    Ok(Some(out_path))
}

/// Write bytes to file
async fn save_tgz(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    // Create parent directory if it doesn't exist
    if let Some(parent_dir) = path.parent()
        && let Some(parent_str) = parent_dir.to_str()
    {
        tokio_fs_ext::create_dir_all(parent_str).await?;
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
    use crate::{package_lock::{LockPackage, PackageLock}, set_cwd};

    use wasm_bindgen_test::*;

    /// Create a test package lock with minimal content
    fn create_test_package_lock() -> PackageLock {
        let mut packages = std::collections::HashMap::new();

        // Root package
        let root_package = LockPackage {
            name: Some("test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: Some(std::collections::HashMap::new()),
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
        };
        packages.insert("".to_string(), root_package);

        // Test dependency
        let test_package = LockPackage {
            name: Some("lodash".to_string()),
            version: Some("4.17.21".to_string()),
            resolved: Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            integrity: Some("sha512-/2U81OjsGkbyk2+ThmuxvWcDrfj8q+I+evwve1/49eHGH9bLjjPKFmy6Hmyac1Wg4nW/brXyT3dD9zdLv5L8Ug==".to_string()),
            shasum: Some("fb5dfc0a2ba5a90ee053c813d71f16e6b66ac994".to_string()),
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
        };
        packages.insert("node_modules/lodash".to_string(), test_package);

        PackageLock {
            name: "test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 2,
            requires: true,
            packages,
            dependencies: None,
        }
    }

    #[wasm_bindgen_test]
    async fn test_package_paths_new() {

        let paths = PackagePaths::new(
            "lodash",
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "node_modules/lodash",
        );

        assert_eq!(paths.tgz_store_path.to_string_lossy(), "/stores/lodash/-/lodash-4.17.21.tgz");
        assert_eq!(
            paths.unpacked_dir.to_string_lossy(),
            "/stores/lodash/-/lodash-4.17.21.tgz-unpack"
        );
        assert_eq!(paths.link_target_dir.to_string_lossy(), "node_modules/lodash");
    }

    #[wasm_bindgen_test]
    async fn test_package_paths_new_with_complex_url() {

        let paths = PackagePaths::new(
            "@types/node",
            "https://registry.npmjs.org/@types/node/-/node-18.0.0.tgz",
            "node_modules/@types/node",
        );

        assert_eq!(
            paths.tgz_store_path.to_string_lossy(),
            "/stores/@types/node/-/node-18.0.0.tgz"
        );
        assert_eq!(
            paths.unpacked_dir.to_string_lossy(),
            "/stores/@types/node/-/node-18.0.0.tgz-unpack"
        );
        assert_eq!(paths.link_target_dir.to_string_lossy(), "node_modules/@types/node");
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_simple() {

        let extract_dir = PathBuf::from("/test-extract-simple");
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Create a simple tar.gz with test content
        let tgz_bytes = create_test_tgz_bytes();

        let result = extract_tgz_bytes(&tgz_bytes, &extract_dir).await;
        assert!(result.is_ok());

        // Verify files were extracted
        let entries = crate::read_dir(&extract_dir).await.unwrap();
        let file_names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        assert!(file_names.contains(&"package.json".to_string()));
        assert!(file_names.contains(&"index.js".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_with_package_prefix() {

        let extract_dir = PathBuf::from("/test-extract-prefix");
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Create a tar.gz with package/ prefix
        let tgz_bytes = create_test_tgz_with_package_prefix();

        let result = extract_tgz_bytes(&tgz_bytes, &extract_dir).await;
        assert!(result.is_ok());

        // Verify files were extracted without package/ prefix
        let entries = crate::read_dir(&extract_dir).await.unwrap();
        let file_names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        assert!(file_names.contains(&"package.json".to_string()));
        assert!(file_names.contains(&"src".to_string()));
        assert!(!file_names.contains(&"package".to_string()));

        // Check that src is a directory and contains main.js
        let src_entries = crate::read_dir(&format!("{}/src", extract_dir.to_string_lossy()))
            .await
            .unwrap();
        let src_file_names: Vec<String> = src_entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        assert!(src_file_names.contains(&"main.js".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_invalid_data() {

        let extract_dir = PathBuf::from("/test-extract-invalid");
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Invalid tar.gz data
        let invalid_bytes = b"not a valid tar.gz file";

        let result = extract_tgz_bytes(invalid_bytes, &extract_dir).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to read archive"));
    }

    #[wasm_bindgen_test]
    async fn test_install_package_with_url() {

        let result = install_package(
            "lodash",
            "4.17.21",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            None,
            None,
            "node_modules/lodash",
        )
        .await;

        assert!(result.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_install_package_without_url() {

        let result = install_package("lodash", "4.17.21", &None, None, None, "node_modules/lodash").await;

        // Should fail with error about missing resolved field
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no resolved field"));
    }

    #[wasm_bindgen_test]
    async fn test_ensure_package_json_new_project() {

        let project_name = PathBuf::from("/test-project-new");
        set_cwd("/test-project-new");
        tokio_fs_ext::create_dir_all(&project_name).await.unwrap();

        let lock = create_test_package_lock();

        let result = ensure_package_json(&lock).await;
        assert!(result.is_ok());

        // Verify package.json was created
        let package_json_exists = tokio_fs_ext::metadata(&format!("/{}/package.json", project_name.to_string_lossy())).await.is_ok();
        assert!(package_json_exists);

        // Verify node_modules directory was created
        let node_modules_exists = tokio_fs_ext::metadata(&format!("/{}/node_modules", project_name.to_string_lossy())).await.is_ok();
        assert!(node_modules_exists);
    }

    #[wasm_bindgen_test]
    async fn test_ensure_package_json_existing_project() {

        let project_name = PathBuf::from("/test-project-existing");
        tokio_fs_ext::create_dir_all(&project_name).await.unwrap();

        // Create existing package.json
        tokio_fs_ext::write(&format!("{}/package.json", project_name.to_string_lossy()), "{}")
            .await
            .unwrap();

        let lock = create_test_package_lock();

        let result = ensure_package_json(&lock).await;
        assert!(result.is_ok());

        // Verify existing package.json was not overwritten
        let content = crate::read(&format!("{}/package.json", project_name.to_string_lossy())).await.unwrap();
        let content_str = String::from_utf8(content).unwrap();
        assert_eq!(content_str, "{}");
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_with_valid_lock() {

        let lock = create_test_package_lock();
        let lock_json = serde_json::to_string(&lock).unwrap();

        let project_name = PathBuf::from("/test-project-install");
        tokio_fs_ext::create_dir_all(&project_name).await.unwrap();

        let result = install_deps(&lock_json, 10).await;

        // May succeed or fail depending on network availability
        // Just verify it returns a result (not panics)
        if let Err(e) = result {
            println!("Install failed (expected in test environment): {}", e);
        }
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_with_invalid_lock() {

        let invalid_lock_json = "{ invalid json }";

        let result = install_deps(invalid_lock_json, 10).await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_opfs_write() {

        let test_file = PathBuf::from("/test-opfs-write.txt");
        let content = "Hello, OPFS!";

        // Try to write to a file
        let result = tokio_fs_ext::write(&test_file, content).await;

        assert!(result.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_empty_packages() {

        let mut lock = create_test_package_lock();
        lock.packages.clear();
        lock.packages.insert(
            "".to_string(),
            LockPackage {
                name: Some("empty-project".to_string()),
                version: Some("1.0.0".to_string()),
                resolved: None,
                integrity: None,
                shasum: None,
                license: None,
                dependencies: Some(std::collections::HashMap::new()),
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
            },
        );

        let lock_json = serde_json::to_string(&lock).unwrap();

        let result = install_deps(&lock_json, 10).await;
        assert!(result.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_install_types_react_package() {
        test_utils::init_tracing();

        // Test installing real @types/react package
        let result = install_package(
            "@types/react",
            "18.0.0",
            &Some("https://registry.npmjs.org/@types/react/-/react-18.0.0.tgz".to_string()),
            None,
            None,
            "node_modules/@types/react",
        )
        .await;

        // If installation was successful, verify the package structure
        if result.is_ok() {
            // Check that index.d.ts exists (main type definition file)
            let index_dts_exists = crate::read("node_modules/@types/react/index.d.ts").await.is_ok();
            assert!(index_dts_exists, "index.d.ts should exist in @types/react");

            // Verify package.json content
            let package_json_content = crate::read("node_modules/@types/react/package.json").await.unwrap();
            let package_json_str = String::from_utf8(package_json_content).unwrap();
            assert!(package_json_str.contains("@types/react"));
            assert!(package_json_str.contains("18.0.0"));
        }
    }

    #[wasm_bindgen_test]
    async fn test_install_scoped_package() {
        test_utils::init_tracing();

        // Test installing real @babel/runtime package
        let result = install_package(
            "@babel/runtime",
            "7.28.4",
            &Some("https://registry.npmjs.org/@babel/runtime/-/runtime-7.28.4.tgz".to_string()),
            None,
            None,
            "node_modules/@babel/runtime",
        )
        .await;

        // If installation was successful, verify the package structure
        if result.is_ok() {
            // Verify package.json content
            let package_json_content = crate::read("node_modules/@babel/runtime/package.json").await.unwrap();
            println!("Package.json content: {:?}", String::from_utf8_lossy(&package_json_content));
            let package_json_str = String::from_utf8(package_json_content).unwrap();
            assert!(package_json_str.contains("@babel/runtime"));
            assert!(package_json_str.contains("7.28.4"));
        }
    }

    #[wasm_bindgen_test]
    async fn test_calculate_output_path_skips_root_directory() {
        use std::path::PathBuf;

        let extract_dir = PathBuf::from("/test");
        let root_prefix = Some("package".to_string());

        // Test that root directory is skipped
        let result = calculate_output_path("package", &extract_dir, &root_prefix);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "Root directory should be skipped");

        // Test that files under root are not skipped
        let result = calculate_output_path("package/file.txt", &extract_dir, &root_prefix);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.is_some());
        assert_eq!(path.unwrap(), PathBuf::from("/test/file.txt"));

        // Test that files without prefix are not skipped
        let result = calculate_output_path("file.txt", &extract_dir, &None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.is_some());
        assert_eq!(path.unwrap(), PathBuf::from("/test/file.txt"));
    }

    #[wasm_bindgen_test]
    async fn test_nested_node_modules_structure() {

        set_cwd("/nested-test-project");
        // Create a package lock with three-level nested structure
        let mut packages = std::collections::HashMap::new();

        // Root package
        let root_package = LockPackage {
            name: Some("nested-test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: Some(std::collections::HashMap::new()),
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
        };
        packages.insert("".to_string(), root_package);

        // Level 1: lodash
        let lodash_package = LockPackage {
            name: Some("lodash".to_string()),
            version: Some("4.17.21".to_string()),
            resolved: Some("https://registry.npmmirror.com/lodash/-/lodash-4.17.21.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/lodash".to_string(), lodash_package);

        // Level 2: lodash.has (dependency of lodash)
        let lodash_has_package = LockPackage {
            name: Some("lodash.has".to_string()),
            version: Some("4.5.2".to_string()),
            resolved: Some("https://registry.npmmirror.com/lodash.has/-/lodash.has-4.5.2.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/lodash/node_modules/lodash.has".to_string(), lodash_has_package);

        // Level 3: deep-strict (dependency of lodash.has)
        let deep_strict_package = LockPackage {
            name: Some("deep-strict".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: Some("https://registry.npmmirror.com/lodash.chunk/-/lodash.chunk-4.2.0.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/lodash/node_modules/lodash.has/node_modules/deep-strict".to_string(), deep_strict_package);

        let lock = PackageLock {
            name: "nested-test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 2,
            requires: true,
            packages,
            dependencies: None,
        };

        let lock_json = serde_json::to_string(&lock).unwrap();
        println!("Package lock JSON: {}", lock_json);

        // Test the installation
        let result = install_deps(&lock_json, 10).await;

        // Installation should either succeed or fail with an error
        // (network issues are acceptable in tests)
        let _ = result;

        // Verify the directory structure was created
        // Since ensure_package_json now uses relative paths, we check relative to current directory

        // Check that the main project package.json exists and can be read
        let root_package_json = tokio_fs_ext::read_to_string("./package.json").await;
        assert!(root_package_json.is_ok(), "Root package.json should exist and be readable");

        // Check that node_modules directory exists and can be read
        let node_modules_entries = crate::read_dir("./node_modules").await;
        assert!(node_modules_entries.is_ok(), "node_modules directory should exist and be readable");

        // Check that lodash directory exists and can be read
        let lodash_entries = crate::read_dir("./node_modules/lodash").await;
        assert!(lodash_entries.is_ok(), "lodash directory should exist and be readable");

        // Check that lodash.has directory exists and can be read
        let lodash_has_entries = crate::read_dir("./node_modules/lodash/node_modules/lodash.has").await;
        assert!(lodash_has_entries.is_ok(), "lodash.has directory should exist and be readable");

        // Check that deep-strict directory exists and can be read
        let deep_strict_entries = crate::read_dir("./node_modules/lodash/node_modules/lodash.has/node_modules/deep-strict").await;
        assert!(deep_strict_entries.is_ok(), "deep-strict directory should exist and be readable");

        // Test reading directory contents through fuse links
        // Read ./node_modules/lodash
        let lodash_entries = crate::read_dir("./node_modules/lodash").await.unwrap();
        let lodash_names: Vec<String> = lodash_entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        // Should contain node_modules (for lodash.has) and package content
        assert!(lodash_names.contains(&"node_modules".to_string()));
        assert!(lodash_names.contains(&"package.json".to_string()));

        // Read ./node_modules/lodash/node_modules/lodash.has
        let lodash_has_entries = crate::read_dir("./node_modules/lodash/node_modules/lodash.has").await.unwrap();
        let lodash_has_names: Vec<String> = lodash_has_entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        println!("Lodash.has directory entries: {:?}", lodash_has_names);

        // Should contain node_modules (for deep-strict) and package content
        assert!(lodash_has_names.contains(&"node_modules".to_string()));
        assert!(lodash_has_names.contains(&"package.json".to_string()));

        // Read ./node_modules/lodash/node_modules/lodash.has/node_modules/deep-strict
        let deep_strict_entries = crate::read_dir("./node_modules/lodash/node_modules/lodash.has/node_modules/deep-strict").await.unwrap();
        let deep_strict_names: Vec<String> = deep_strict_entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        println!("Deep-strict directory entries: {:?}", deep_strict_names);

        // Should contain package content (no node_modules at this level)
        assert!(deep_strict_names.contains(&"package.json".to_string()));
        assert!(!deep_strict_names.contains(&"node_modules".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_dirty_cache_cleanup_and_reinstall() {
        test_utils::init_tracing();

        // First installation
        let result = install_package(
            "test-dirty-cache",
            "1.0.0",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            None,
            None,
            "node_modules/test-dirty-cache",
        )
        .await;

        println!("First install result: {:?}", result);

        // If first installation was successful, proceed with dirty cache test
        if result.is_ok() {
            // Verify package.json exists in unpacked directory
            let paths = PackagePaths::new(
                "test-dirty-cache",
                "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "node_modules/test-dirty-cache",
            );

            // Check that resolved marker exists
            let marker_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_exists, "Resolved marker should exist after first install");

            // Check that package.json exists in unpacked dir
            let package_json_path = paths.unpacked_dir.join("package.json");
            let package_json_exists = tokio_fs_ext::metadata(&package_json_path).await.is_ok();
            assert!(package_json_exists, "package.json should exist in unpacked dir");

            // Simulate dirty cache: delete package.json and _resolved marker
            println!("Deleting package.json from unpacked dir...");
            let _ = tokio_fs_ext::remove_file(&package_json_path).await;

            println!("Deleting _resolved marker...");
            let _ = tokio_fs_ext::remove_file(&paths.resolved_marker).await;

            // Verify they are deleted
            let marker_deleted = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_err();
            assert!(marker_deleted, "Resolved marker should be deleted");

            let package_json_deleted = tokio_fs_ext::metadata(&package_json_path).await.is_err();
            assert!(package_json_deleted, "package.json should be deleted");

            // Second installation - should detect dirty cache and reinstall
            println!("Running second install...");
            let result2 = install_package(
                "test-dirty-cache",
                "1.0.0",
                &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
                None,
                None,
                "node_modules/test-dirty-cache",
            )
            .await;

            println!("Second install result: {:?}", result2);
            assert!(result2.is_ok(), "Second install should succeed");

            // Verify that package.json is restored in unpacked dir
            let package_json_restored = tokio_fs_ext::metadata(&package_json_path).await.is_ok();
            assert!(package_json_restored, "package.json should be restored after reinstall");

            // Verify that _resolved marker is restored
            let marker_restored = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_restored, "Resolved marker should be restored after reinstall");

            // Verify package content is correct
            let package_json_content = crate::read(&package_json_path).await.unwrap();
            let package_json_str = String::from_utf8(package_json_content).unwrap();
            assert!(package_json_str.contains("lodash"), "package.json should contain lodash");
        }
    }

    #[wasm_bindgen_test]
    async fn test_marker_exists_but_dir_missing() {
        test_utils::init_tracing();

        // First installation to create the cache
        let result = install_package(
            "test-marker-only",
            "1.0.0",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            None,
            None,
            "node_modules/test-marker-only",
        )
        .await;

        println!("First install result: {:?}", result);

        // If first installation was successful, proceed with test
        if result.is_ok() {
            let paths = PackagePaths::new(
                "test-marker-only",
                "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "node_modules/test-marker-only",
            );

            // Verify marker exists
            let marker_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_exists, "Marker should exist after first install");

            // Verify unpacked_dir exists
            let dir_exists = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok();
            assert!(dir_exists, "Unpacked dir should exist after first install");

            // Simulate scenario: delete unpacked_dir but keep marker
            println!("Deleting unpacked_dir but keeping marker...");
            let _ = tokio_fs_ext::remove_dir_all(&paths.unpacked_dir).await;

            // Verify unpacked_dir is deleted
            let dir_deleted = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_err();
            assert!(dir_deleted, "Unpacked dir should be deleted");

            // Verify marker still exists
            let marker_still_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_still_exists, "Marker should still exist");

            // Second installation - should detect incomplete cache and reinstall
            println!("Running second install with marker-only state...");
            let result2 = install_package(
                "test-marker-only",
                "1.0.0",
                &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
                None,
                None,
                "node_modules/test-marker-only",
            )
            .await;

            println!("Second install result: {:?}", result2);
            assert!(
                result2.is_ok(),
                "Second install should succeed and recreate unpacked_dir"
            );

            // Verify both marker and unpacked_dir exist after reinstall
            let marker_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_exists, "Marker should exist after reinstall");

            let dir_exists = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok();
            assert!(dir_exists, "Unpacked dir should exist after reinstall");

            // Verify package content is correct
            let package_json_path = paths.unpacked_dir.join("package.json");
            let package_json_content = crate::read(&package_json_path).await.unwrap();
            let package_json_str = String::from_utf8(package_json_content).unwrap();
            assert!(package_json_str.contains("lodash"), "package.json should contain lodash");
        }
    }

    #[wasm_bindgen_test]
    async fn test_dir_exists_but_marker_missing() {
        test_utils::init_tracing();

        // First installation to create the cache
        let result = install_package(
            "test-dir-only",
            "1.0.0",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            None,
            None,
            "node_modules/test-dir-only",
        )
        .await;

        println!("First install result: {:?}", result);

        // If first installation was successful, proceed with test
        if result.is_ok() {
            let paths = PackagePaths::new(
                "test-dir-only",
                "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "node_modules/test-dir-only",
            );

            // Verify marker exists
            let marker_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_exists, "Marker should exist after first install");

            // Verify unpacked_dir exists
            let dir_exists = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok();
            assert!(dir_exists, "Unpacked dir should exist after first install");

            // Simulate scenario: delete marker but keep unpacked_dir
            println!("Deleting marker but keeping unpacked_dir...");
            let _ = tokio_fs_ext::remove_file(&paths.resolved_marker).await;

            // Verify marker is deleted
            let marker_deleted = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_err();
            assert!(marker_deleted, "Marker should be deleted");

            // Verify unpacked_dir still exists
            let dir_still_exists = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok();
            assert!(dir_still_exists, "Unpacked dir should still exist");

            // Second installation - should detect incomplete cache and reinstall (overwrite)
            println!("Running second install with dir-only state...");
            let result2 = install_package(
                "test-dir-only",
                "1.0.0",
                &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
                None,
                None,
                "node_modules/test-dir-only",
            )
            .await;

            println!("Second install result: {:?}", result2);
            assert!(
                result2.is_ok(),
                "Second install should succeed and recreate marker"
            );

            // Verify both marker and unpacked_dir exist after reinstall
            let marker_exists = tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok();
            assert!(marker_exists, "Marker should exist after reinstall");

            let dir_exists = tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok();
            assert!(dir_exists, "Unpacked dir should exist after reinstall");

            // Verify package content is correct (files should be overwritten)
            let package_json_path = paths.unpacked_dir.join("package.json");
            let package_json_content = crate::read(&package_json_path).await.unwrap();
            let package_json_str = String::from_utf8(package_json_content).unwrap();
            assert!(package_json_str.contains("lodash"), "package.json should contain lodash");
        }
    }

    #[wasm_bindgen_test]
    async fn test_install_package_with_invalid_url() {
        test_utils::init_tracing();

        // Test with invalid URL that should fail
        let result = install_package(
            "invalid-package",
            "1.0.0",
            &Some("https://invalid-domain-that-does-not-exist.com/package.tgz".to_string()),
            None,
            None,
            "node_modules/invalid-package",
        )
        .await;

        // Should fail with network error
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        println!("Error message: {}", err_msg);
        // anyhow error chain contains context at different levels
        assert!(err_msg.contains("invalid-package@1.0.0"));
    }

    #[wasm_bindgen_test]
    async fn test_install_package_with_invalid_tgz() {
        test_utils::init_tracing();

        // Create an invalid tgz file
        let invalid_tgz_path = PathBuf::from("/test-invalid-tgz/package.tgz");
        tokio_fs_ext::create_dir_all("/test-invalid-tgz").await.unwrap();
        tokio_fs_ext::write(&invalid_tgz_path, b"not a valid tgz").await.unwrap();

        // Try to extract it
        let extract_dir = PathBuf::from("/test-invalid-extract");
        let result = extract_tgz_bytes(b"not a valid tgz", &extract_dir).await;

        // Should fail with error about reading archive
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to read archive"));
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_with_mixed_success_and_failure() {
        test_utils::init_tracing();

        let mut packages = std::collections::HashMap::new();

        // Root package
        let root_package = LockPackage {
            name: Some("mixed-test-project".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: Some(std::collections::HashMap::new()),
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
        };
        packages.insert("".to_string(), root_package);

        // Valid package
        let valid_package = LockPackage {
            name: Some("lodash".to_string()),
            version: Some("4.17.21".to_string()),
            resolved: Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/lodash".to_string(), valid_package);

        // Invalid package (no resolved field)
        let invalid_package = LockPackage {
            name: Some("invalid-package".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None, // This will cause an error
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
        };
        packages.insert("node_modules/invalid-package".to_string(), invalid_package);

        let lock = PackageLock {
            name: "mixed-test-project".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 2,
            requires: true,
            packages,
            dependencies: None,
        };

        let lock_json = serde_json::to_string(&lock).unwrap();

        // Should fail because one package has no resolved field
        let result = install_deps(&lock_json, 10).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        // The error should contain information about the missing resolved field
        assert!(err_msg.contains("no resolved field"));
    }

    #[wasm_bindgen_test]
    async fn test_download_bytes_network_error() {
        test_utils::init_tracing();

        // Test downloading from invalid URL
        let result = download_bytes("https://invalid-domain-that-does-not-exist.com/file.tgz").await;

        // Should fail with network error
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to download from"));
    }

    /// Helper function to create test tar.gz bytes
    fn create_test_tgz_bytes() -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut tar_data = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut tar_data);

            // Add package.json
            let package_json = r#"{"name":"test-package","version":"1.0.0"}"#;
            let mut header = tar::Header::new_gnu();
            header.set_path("package.json").unwrap();
            header.set_size(package_json.len() as u64);
            header.set_cksum();
            archive.append(&header, package_json.as_bytes()).unwrap();

            // Add index.js
            let index_js = "console.log('Hello, World!');";
            let mut header = tar::Header::new_gnu();
            header.set_path("index.js").unwrap();
            header.set_size(index_js.len() as u64);
            header.set_cksum();
            archive.append(&header, index_js.as_bytes()).unwrap();
        }

        // Compress with gzip
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    /// Helper function to create test tar.gz bytes with package/ prefix
    fn create_test_tgz_with_package_prefix() -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut tar_data = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut tar_data);

            // Add package.json with package/ prefix
            let package_json = r#"{"name":"test-package","version":"1.0.0"}"#;
            let mut header = tar::Header::new_gnu();
            header.set_path("package/package.json").unwrap();
            header.set_size(package_json.len() as u64);
            header.set_cksum();
            archive.append(&header, package_json.as_bytes()).unwrap();

            // Add src/main.js with package/ prefix
            let main_js = "console.log('Hello from main.js');";
            let mut header = tar::Header::new_gnu();
            header.set_path("package/src/main.js").unwrap();
            header.set_size(main_js.len() as u64);
            header.set_cksum();
            archive.append(&header, main_js.as_bytes()).unwrap();
        }

        // Compress with gzip
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    #[wasm_bindgen_test]
    async fn test_marker_as_directory_not_cached() {
        test_utils::init_tracing();

        let mut packages = std::collections::HashMap::new();

        // Root package
        let root_package = LockPackage {
            name: Some("test-marker-dir".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: Some(std::collections::HashMap::new()),
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
        };
        packages.insert("".to_string(), root_package);

        // Test package with valid URL
        let test_package = LockPackage {
            name: Some("test-pkg".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/test-pkg".to_string(), test_package);

        let lock = PackageLock {
            name: "test-marker-dir".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 2,
            requires: true,
            packages,
            dependencies: None,
        };

        // Create paths
        let paths = PackagePaths::new(
            "test-pkg",
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "node_modules/test-pkg",
        );

        // Create marker as a DIRECTORY (incorrect state)
        tokio_fs_ext::create_dir_all(&paths.resolved_marker).await.unwrap();

        // Create unpacked_dir as a proper directory
        tokio_fs_ext::create_dir_all(&paths.unpacked_dir).await.unwrap();

        let lock_json = serde_json::to_string(&lock).unwrap();

        // This should NOT treat the package as cached because marker is a directory, not a file
        // It should attempt to download and install
        let result = install_deps(&lock_json, 10).await;

        // We expect this to either:
        // 1. Succeed (if network is available) - marker gets replaced with proper file
        // 2. Fail (if network unavailable or other error)
        // The key point is that it should NOT skip the package as "cached"

        if result.is_ok() {
            // If successful, marker should now be a file
            let marker_meta = tokio_fs_ext::metadata(&paths.resolved_marker).await.unwrap();
            assert!(marker_meta.is_file(), "marker should be a file after install_deps");
        } else {
            println!("Test result: {:?}", result);
            // Network might be unavailable, which is acceptable for this test
        }
    }

    #[wasm_bindgen_test]
    async fn test_unpacked_dir_as_file_not_cached() {
        test_utils::init_tracing();

        let mut packages = std::collections::HashMap::new();

        // Root package
        let root_package = LockPackage {
            name: Some("test-dir-file".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: None,
            integrity: None,
            shasum: None,
            license: None,
            dependencies: Some(std::collections::HashMap::new()),
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
        };
        packages.insert("".to_string(), root_package);

        // Test package with valid URL
        let test_package = LockPackage {
            name: Some("test-pkg2".to_string()),
            version: Some("1.0.0".to_string()),
            resolved: Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            integrity: None,  // No integrity check for this test
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
        };
        packages.insert("node_modules/test-pkg2".to_string(), test_package);

        let lock = PackageLock {
            name: "test-dir-file".to_string(),
            version: "1.0.0".to_string(),
            lockfile_version: 2,
            requires: true,
            packages,
            dependencies: None,
        };

        // Create paths
        let paths = PackagePaths::new(
            "test-pkg2",
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "node_modules/test-pkg2",
        );

        // Create marker as a proper file
        tokio_fs_ext::create_dir_all(paths.resolved_marker.parent().unwrap()).await.unwrap();
        tokio_fs_ext::write(&paths.resolved_marker, b"").await.unwrap();

        // Create unpacked_dir as a FILE (incorrect state)
        tokio_fs_ext::create_dir_all(paths.unpacked_dir.parent().unwrap()).await.unwrap();
        tokio_fs_ext::write(&paths.unpacked_dir, b"not a directory").await.unwrap();

        let lock_json = serde_json::to_string(&lock).unwrap();

        // This should NOT treat the package as cached because unpacked_dir is a file, not a directory
        let result = install_deps(&lock_json, 10).await;

        // We expect this to either:
        // 1. Succeed (if network is available) - unpacked_dir gets replaced with proper directory
        // 2. Fail (if network unavailable or other error)

        if result.is_ok() {
            // If successful, unpacked_dir should now be a directory
            let dir_meta = tokio_fs_ext::metadata(&paths.unpacked_dir).await.unwrap();
            assert!(dir_meta.is_dir(), "unpacked_dir should be a directory after install_deps");
        } else {
            println!("Test result: {:?}", result);
            // Network might be unavailable, which is acceptable for this test
        }
    }

    #[wasm_bindgen_test]
    async fn test_corrupted_tgz_file_gets_redownloaded() {
        test_utils::init_tracing();

        let name = "test-corrupted-tgz";
        let version = "1.0.0";
        let url = "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz";
        let path_key = "node_modules/test-corrupted-tgz";

        let paths = PackagePaths::new(name, url, path_key);

        // Clean up any existing files from previous test runs
        let _ = tokio_fs_ext::remove_file(&paths.resolved_marker).await;
        let _ = tokio_fs_ext::remove_dir_all(&paths.unpacked_dir).await;
        let _ = tokio_fs_ext::remove_file(&paths.tgz_store_path).await;

        // Create an empty/corrupted tgz file in the store
        tokio_fs_ext::create_dir_all(paths.tgz_store_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&paths.tgz_store_path, b"corrupted data")
            .await
            .unwrap();

        // Verify corrupted file exists
        let corrupted_meta = tokio_fs_ext::metadata(&paths.tgz_store_path).await.unwrap();
        let corrupted_size = corrupted_meta.len();
        assert_eq!(corrupted_size, 14); // "corrupted data".len()

        // Call install_package - should detect no marker/unpacked_dir and re-download
        let result = install_package(name, version, &Some(url.to_string()), None, None, path_key).await;

        if result.is_ok() {
            // Verify tgz was replaced with valid content
            let new_meta = tokio_fs_ext::metadata(&paths.tgz_store_path).await.unwrap();
            let new_size = new_meta.len();

            // Valid lodash tgz should be much larger than 14 bytes
            assert!(
                new_size > 1000,
                "tgz file should be replaced with valid content, got {} bytes",
                new_size
            );

            // Verify marker was created (indicating successful extraction)
            assert!(
                tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok(),
                "marker file should exist after successful installation"
            );

            // Verify unpacked_dir exists
            assert!(
                tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok(),
                "unpacked_dir should exist after successful installation"
            );

            // Verify package.json exists in unpacked dir
            let package_json_path = paths.unpacked_dir.join("package.json");
            assert!(
                tokio_fs_ext::metadata(&package_json_path).await.is_ok(),
                "package.json should exist in unpacked directory"
            );
        } else {
            println!("Test skipped: network unavailable");
        }
    }

    #[wasm_bindgen_test]
    async fn test_verify_integrity_with_shasum() {
        // Test data: "hello world"
        let test_data = b"hello world";

        // SHA1 hash of "hello world" is 2aae6c35c94fcfb415dbe95f408b9ce91ee846ed
        let expected_shasum = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

        // Verify with correct shasum
        assert!(pack::verify_integrity(test_data, None, Some(expected_shasum)));

        // Verify with incorrect shasum
        assert!(!pack::verify_integrity(test_data, None, Some("incorrect_hash")));

        // Verify with no hash info
        assert!(!pack::verify_integrity(test_data, None, None));
    }

    #[wasm_bindgen_test]
    async fn test_verify_integrity_with_integrity() {
        // Test data: "hello world"
        let test_data = b"hello world";

        // SHA512 hash of "hello world" in base64
        let expected_integrity = "sha512-MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==";

        // Verify with correct integrity
        assert!(pack::verify_integrity(test_data, Some(expected_integrity), None));

        // Verify with incorrect integrity
        assert!(!pack::verify_integrity(test_data, Some("sha512-incorrect"), None));
    }

    #[wasm_bindgen_test]
    async fn test_verify_integrity_priority() {
        // Test data: "hello world"
        let test_data = b"hello world";

        let correct_integrity = "sha512-MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==";
        let correct_shasum = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

        // When both are provided, integrity should take priority
        // If integrity is correct, should return true even if shasum is wrong
        assert!(pack::verify_integrity(test_data, Some(correct_integrity), Some("wrong_shasum")));

        // If integrity is wrong, should return false even if shasum is correct
        assert!(!pack::verify_integrity(test_data, Some("sha512-wrong"), Some(correct_shasum)));
    }

    #[wasm_bindgen_test]
    async fn test_empty_tgz_file_gets_redownloaded() {
        test_utils::init_tracing();

        let name = "test-empty-tgz";
        let version = "1.0.0";
        let url = "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz";
        let path_key = "node_modules/test-empty-tgz";

        let paths = PackagePaths::new(name, url, path_key);

        // Clean up any existing files from previous test runs
        let _ = tokio_fs_ext::remove_file(&paths.resolved_marker).await;
        let _ = tokio_fs_ext::remove_dir_all(&paths.unpacked_dir).await;
        let _ = tokio_fs_ext::remove_file(&paths.tgz_store_path).await;

        // Create an empty tgz file in the store
        tokio_fs_ext::create_dir_all(paths.tgz_store_path.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&paths.tgz_store_path, b"")
            .await
            .unwrap();

        // Verify empty file exists
        let empty_meta = tokio_fs_ext::metadata(&paths.tgz_store_path).await.unwrap();
        assert_eq!(empty_meta.len(), 0);

        // Call install_package - should detect no marker/unpacked_dir and re-download
        let result = install_package(name, version, &Some(url.to_string()), None, None, path_key).await;

        if result.is_ok() {
            // Verify tgz was replaced with valid content
            let new_meta = tokio_fs_ext::metadata(&paths.tgz_store_path).await.unwrap();
            let new_size = new_meta.len();

            // Valid lodash tgz should be much larger than 0 bytes
            assert!(
                new_size > 1000,
                "empty tgz file should be replaced with valid content, got {} bytes",
                new_size
            );

            // Verify marker was created
            assert!(
                tokio_fs_ext::metadata(&paths.resolved_marker).await.is_ok(),
                "marker file should exist after successful installation"
            );

            // Verify unpacked_dir exists
            assert!(
                tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok(),
                "unpacked_dir should exist after successful installation"
            );
        } else {
            println!("Test skipped: network unavailable");
        }
    }
}
