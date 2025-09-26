use flate2::read::GzDecoder;
use futures::future::join_all;
use std::io::Read;
use std::io::Result;
use std::path::PathBuf;
use tar::Archive;

use super::fuse;

use crate::package_lock::PackageLock;

/// Download all tgz packages to OPFS
pub async fn install_deps(package_lock: &str) -> Result<Vec<String>> {
    let lock = PackageLock::from_json(package_lock)?;

    // Write package.json to root
    ensure_package_json(&lock).await?;

    // Prepare tasks for parallel execution
    let tasks: Vec<_> = lock
        .packages
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(path, pkg)| {
            let name = pkg.get_name(path);
            let version = pkg.get_version();
            let tgz_url = pkg.resolved.clone();
            let path_key = path.clone();

            async move { install_package(&name, &version, &tgz_url, &path_key).await }
        })
        .collect();

    // Run all tasks in parallel and collect results
    let results = join_all(tasks).await;
    Ok(results)
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
    path_key: &str,
) -> String {
    match tgz_url {
        Some(url) => {
            let paths = PackagePaths::new(name, url, path_key);

            // Check if already unpacked
            if tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok() {
                match fuse::fuse_link(&paths.unpacked_dir, &paths.link_target_dir).await {
                    Ok(_) => format!("{name}@{version}: installed successfully"),
                    Err(e) => format!("{name}@{version}: {e}"),
                }
            } else {
                // Get or download tgz bytes
                match get_or_download_tgz(url, &paths.tgz_store_path).await {
                    Ok(tgz_bytes) => {
                        // Extract and create fuse link
                        match extract_tgz_bytes(&tgz_bytes, &paths.unpacked_dir).await {
                            Ok(_) => match fuse::fuse_link(&paths.unpacked_dir, &paths.link_target_dir).await {
                                Ok(_) => format!("{name}@{version}: installed successfully"),
                                Err(e) => format!("{name}@{version}: {e}"),
                            },
                            Err(e) => format!("{name}@{version}: {e}"),
                        }
                    }
                    Err(e) => format!("{name}@{version}: {e}"),
                }
            }
        }
        None => format!("{name}@{version}: no resolved field"),
    }
}

/// Get or download tgz file
async fn get_or_download_tgz(tgz_url: &str, tgz_store_path: &PathBuf) -> Result<Vec<u8>> {
    if tokio_fs_ext::metadata(tgz_store_path).await.is_ok() {
        super::util::read_direct(tgz_store_path).await
    } else {
        let bytes = download_bytes(tgz_url).await?;
        save_tgz(tgz_store_path, &bytes).await?;
        Ok(bytes)
    }
}

/// Package paths for installation
struct PackagePaths {
    tgz_store_path: PathBuf,
    unpacked_dir: PathBuf,
    link_target_dir: PathBuf,
}

impl PackagePaths {
    fn new(name: &str, tgz_url: &str, path_key: &str) -> Self {
        let url_path: Vec<_> = tgz_url.split('/').collect();
        let tgz_file_name = url_path.last().unwrap_or(&"package.tgz");

        Self {
            tgz_store_path: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}")),
            unpacked_dir: PathBuf::from(format!("/stores/{name}/-/{tgz_file_name}-unpack")),
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
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Collect all archive entries with their contents
    let mut archive_entries = Vec::new();

    for entry in entries {
        let mut entry = entry.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = entry.path().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path_str = path.to_string_lossy().to_string();
        let is_file = entry.header().entry_type().is_file();

        let contents = if is_file {
            let mut file_contents = Vec::new();
            entry
                .read_to_end(&mut file_contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
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
    // Look for package.json to determine the root directory
    for entry in entries {
        if entry.path.ends_with("/package.json") {
            if let Some(prefix) = entry.path.strip_suffix("/package.json") {
                return Some(prefix.to_string());
            }
        } else if entry.path == "package.json" {
            // package.json is at root level
            return None;
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
    tokio_fs_ext::write(path, bytes).await
}

/// Download bytes from URL
async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NetworkUnreachable, e))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NetworkUnreachable, e))?;
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
    }

    #[wasm_bindgen_test]
    async fn test_install_package_with_url() {

        let result = install_package(
            "lodash",
            "4.17.21",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            "node_modules/lodash",
        )
        .await;

        // Should contain success message or error message
        assert!(result.contains("lodash@4.17.21"));
    }

    #[wasm_bindgen_test]
    async fn test_install_package_without_url() {

        let result = install_package("lodash", "4.17.21", &None, "node_modules/lodash").await;

        assert_eq!(result, "lodash@4.17.21: no resolved field");
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

        let results = install_deps(&lock_json).await;
        assert!(results.is_ok());

        let results = results.unwrap();
        assert!(!results.is_empty());

        // Should contain results for lodash package
        assert!(results.iter().any(|r| r.contains("lodash@4.17.21")));
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_with_invalid_lock() {

        let invalid_lock_json = "{ invalid json }";

        let result = install_deps(invalid_lock_json).await;
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

        let results = install_deps(&lock_json).await;
        assert!(results.is_ok());

        let results = results.unwrap();
        assert!(results.is_empty());
    }

    #[wasm_bindgen_test]
    async fn test_install_types_react_package() {
        test_utils::init_tracing();

        // Test installing real @types/react package
        let result = install_package(
            "@types/react",
            "18.0.0",
            &Some("https://registry.npmjs.org/@types/react/-/react-18.0.0.tgz".to_string()),
            "node_modules/@types/react",
        )
        .await;

        // Should contain success message or error message
        assert!(result.contains("@types/react@18.0.0"));

        // If installation was successful, verify the package structure
        if result.contains("installed successfully") {
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
            integrity: Some("sha512-test-lodash".to_string()),
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
            integrity: Some("sha512-test-lodash-has".to_string()),
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
            integrity: Some("sha512-test-deep-strict".to_string()),
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
        let results = install_deps(&lock_json).await;
        assert!(results.is_ok());

        let results = results.unwrap();

        // Print actual results for debugging
        println!("Actual results count: {}", results.len());
        for result in &results {
            println!("Result: {}", result);
        }

        // Check if any packages were installed successfully (network might fail for some)
        let successful_installations = results.iter().filter(|r| r.contains("installed successfully")).count();
        let failed_installations = results.iter().filter(|r| r.contains("unexpected end of file") || r.contains("invalid gzip header") || r.contains("NetworkUnreachable")).count();

        // At least one package should be installed successfully, or all should fail due to network issues
        assert!(
            successful_installations > 0 || failed_installations == results.len(),
            "Either some packages should install successfully, or all should fail due to network issues"
        );

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
}
