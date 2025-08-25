use std::io::Result;
use flate2::read::GzDecoder;
use futures::future::join_all;
use std::io::Read;
use tar::Archive;

use super::fuse;

use crate::package_lock::PackageLock;

/// Download all tgz packages to OPFS
pub async fn install_deps(package_lock: &str) -> Result<Vec<String>> {
    let lock = PackageLock::from_json(package_lock)?;
    let project_name = lock.name.clone();

    // Write package.json to root
    ensure_package_json(&project_name, &lock).await?;

    // Prepare tasks for parallel execution
    let tasks: Vec<_> = lock
        .packages
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(path, pkg)| {
            let name = pkg.get_name(path);
            let version = pkg.get_version();
            let tgz_url = pkg.resolved.clone();
            let project_name = project_name.clone();

            async move { install_single_package(&name, &version, &tgz_url, &project_name).await }
        })
        .collect();

    // Run all tasks in parallel and collect results
    let results = join_all(tasks).await;
    Ok(results)
}

/// Write root package.json to the project directory
async fn ensure_package_json(project_name: &str, lock: &PackageLock) -> Result<()> {
    if tokio_fs_ext::metadata(&format!("{project_name}/package.json")).await.is_ok() {
        return Ok(());
    }

    if let Some(root_pkg) = lock.packages.get("") {
        let pkg_json = serde_json::to_string_pretty(root_pkg).unwrap_or("{}".to_string());
        tokio_fs_ext::create_dir_all(&format!("{project_name}/node_modules")).await?;
        tokio_fs_ext::write(&format!("{project_name}/package.json"), pkg_json.as_bytes()).await?;
    }
    Ok(())
}

/// Install a single package
async fn install_single_package(
    name: &str,
    version: &str,
    tgz_url: &Option<String>,
    project_name: &str,
) -> String {
    match tgz_url {
        Some(url) => match install_package(name, version, url, project_name).await {
            Ok(_) => format!("{name}@{version}: installed successfully"),
            Err(e) => format!("{name}@{version}: {e}"),
        },
        None => format!("{name}@{version}: no resolved field"),
    }
}

/// Install package using the provided URL
async fn install_package(
    name: &str,
    _version: &str,
    tgz_url: &str,
    project_name: &str,
) -> Result<()> {
    let paths = PackagePaths::new(name, tgz_url, project_name);

    // Check if already unpacked
    if tokio_fs_ext::metadata(&paths.unpacked_dir).await.is_ok() {
        fuse::fuse_link(&paths.unpacked_dir, &paths.unpack_dir).await?;
        return Ok(());
    }

    // Get or download tgz bytes
    let tgz_bytes = get_or_download_tgz(tgz_url, &paths.tgz_store_path).await?;

    // Extract and create fuse link
    extract_tgz_bytes(&tgz_bytes, &paths.unpacked_dir).await?;
    fuse::fuse_link(&paths.unpacked_dir, &paths.unpack_dir).await?;

    Ok(())
}

/// Get or download tgz file
async fn get_or_download_tgz(tgz_url: &str, tgz_store_path: &str) -> Result<Vec<u8>> {
    if tokio_fs_ext::metadata(tgz_store_path).await.is_ok() {
        crate::read_without_fuse_link(tgz_store_path).await
    } else {
        let bytes = download_bytes(tgz_url).await?;
        save_tgz(tgz_store_path, &bytes).await?;
        Ok(bytes)
    }
}

/// Package paths for installation
struct PackagePaths {
    tgz_store_path: String,
    unpacked_dir: String,
    unpack_dir: String,
}

impl PackagePaths {
    fn new(name: &str, tgz_url: &str, project_name: &str) -> Self {
        let url_path: Vec<_> = tgz_url.split('/').collect();
        let tgz_file_name = url_path.last().unwrap_or(&"package.tgz");

        Self {
            tgz_store_path: format!("/stores/{name}/-/{tgz_file_name}"),
            unpacked_dir: format!("/stores/{name}/-/{tgz_file_name}-unpack"),
            unpack_dir: format!("{project_name}/node_modules/{name}"),
        }
    }
}

/// Extract tgz bytes to directory
pub async fn extract_tgz_bytes(tgz_bytes: &[u8], extract_dir: &str) -> Result<()> {
    let gz = GzDecoder::new(tgz_bytes);
    let mut archive = Archive::new(gz);
    let entries = archive.entries().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = entry.path().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path_str = path.to_string_lossy().to_string();

        // Remove the first-level "package" directory if present
        let out_path = if let Some(stripped) = path_str.strip_prefix("package/") {
            format!("{extract_dir}/{stripped}")
        } else if path_str == "package" {
            // Skip the root package directory
            continue;
        } else {
            format!("{extract_dir}/{path_str}")
        };

        if entry.header().entry_type().is_file() {
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            // Write the file to the output path
            save_tgz(&out_path, &contents).await?;
        }
    }
    Ok(())
}

/// Write bytes to file
async fn save_tgz(path: &str, bytes: &[u8]) -> Result<()> {
    // Create parent directory if it doesn't exist
    if let Some(parent_dir) = std::path::Path::new(path).parent()
        && let Some(parent_str) = parent_dir.to_str()
    {
        tokio_fs_ext::create_dir_all(parent_str).await?;
    }
    tokio_fs_ext::write(path, bytes).await
}

/// Download bytes from URL
async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::get(url).await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let bytes = response.bytes().await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(bytes.to_vec())
}
#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use crate::package_lock::{LockPackage, PackageLock};


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
            "test-project",
        );

        assert_eq!(paths.tgz_store_path, "/stores/lodash/-/lodash-4.17.21.tgz");
        assert_eq!(
            paths.unpacked_dir,
            "/stores/lodash/-/lodash-4.17.21.tgz-unpack"
        );
        assert_eq!(paths.unpack_dir, "test-project/node_modules/lodash");
    }

    #[wasm_bindgen_test]
    async fn test_package_paths_new_with_complex_url() {
        let paths = PackagePaths::new(
            "@types/node",
            "https://registry.npmjs.org/@types/node/-/node-18.0.0.tgz",
            "my-project",
        );

        assert_eq!(
            paths.tgz_store_path,
            "/stores/@types/node/-/node-18.0.0.tgz"
        );
        assert_eq!(
            paths.unpacked_dir,
            "/stores/@types/node/-/node-18.0.0.tgz-unpack"
        );
        assert_eq!(paths.unpack_dir, "my-project/node_modules/@types/node");
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_simple() {
        let extract_dir = "/test-extract-simple".to_string();
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Create a simple tar.gz with test content
        let tgz_bytes = create_test_tgz_bytes();

        let result = extract_tgz_bytes(&tgz_bytes, &extract_dir).await;
        assert!(result.is_ok());

        // Verify files were extracted
        let entries = crate::read_dir(&extract_dir).await.unwrap();
        let file_names: Vec<String> = entries.iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        assert!(file_names.contains(&"package.json".to_string()));
        assert!(file_names.contains(&"index.js".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_with_package_prefix() {
        let extract_dir = "/test-extract-prefix".to_string();
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Create a tar.gz with package/ prefix
        let tgz_bytes = create_test_tgz_with_package_prefix();

        let result = extract_tgz_bytes(&tgz_bytes, &extract_dir).await;
        assert!(result.is_ok());

        // Verify files were extracted without package/ prefix
        let entries = crate::read_dir(&extract_dir).await.unwrap();
        let file_names: Vec<String> = entries.iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        assert!(file_names.contains(&"package.json".to_string()));
        assert!(file_names.contains(&"src".to_string()));
        assert!(!file_names.contains(&"package".to_string()));

        // Check that src is a directory and contains main.js
        let src_entries = crate::read_dir(&format!("{}/src", extract_dir)).await.unwrap();
        let src_file_names: Vec<String> = src_entries.iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        assert!(src_file_names.contains(&"main.js".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_bytes_invalid_data() {
        let extract_dir = "/test-extract-invalid".to_string();
        tokio_fs_ext::create_dir_all(&extract_dir).await.unwrap();

        // Invalid tar.gz data
        let invalid_bytes = b"not a valid tar.gz file";

        let result = extract_tgz_bytes(invalid_bytes, &extract_dir).await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_install_single_package_with_url() {
        let result = install_single_package(
            "lodash",
            "4.17.21",
            &Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            "test-project",
        )
        .await;

        // Should contain success message or error message
        assert!(result.contains("lodash@4.17.21"));
    }

    #[wasm_bindgen_test]
    async fn test_install_single_package_without_url() {
        let result = install_single_package("lodash", "4.17.21", &None, "test-project").await;

        assert_eq!(result, "lodash@4.17.21: no resolved field");
    }

    #[wasm_bindgen_test]
    async fn test_ensure_package_json_new_project() {
        let project_name = "/test-project-new".to_string();
        tokio_fs_ext::create_dir_all(&project_name).await.unwrap();

        let lock = create_test_package_lock();

        let result = ensure_package_json(&project_name, &lock).await;
        assert!(result.is_ok());

        // Verify package.json was created
        let package_json_exists = tokio_fs_ext::metadata(&format!("{}/package.json", project_name))
            .await
            .is_ok();
        assert!(package_json_exists);

        // Verify node_modules directory was created
        let node_modules_exists = tokio_fs_ext::metadata(&format!("{}/node_modules", project_name))
            .await
            .is_ok();
        assert!(node_modules_exists);
    }

    #[wasm_bindgen_test]
    async fn test_ensure_package_json_existing_project() {
        let project_name = "/test-project-existing".to_string();
        tokio_fs_ext::create_dir_all(&project_name).await.unwrap();

        // Create existing package.json
        tokio_fs_ext::write(&format!("{}/package.json", project_name), "{}")
            .await
            .unwrap();

        let lock = create_test_package_lock();

        let result = ensure_package_json(&project_name, &lock).await;
        assert!(result.is_ok());

        // Verify existing package.json was not overwritten
        let content = crate::read(&format!("{}/package.json", project_name))
            .await
            .unwrap();
        let content_str = String::from_utf8(content).unwrap();
        assert_eq!(content_str, "{}");
    }

    #[wasm_bindgen_test]
    async fn test_install_deps_with_valid_lock() {
        let lock = create_test_package_lock();
        let lock_json = serde_json::to_string(&lock).unwrap();

        let project_name = "/test-project-install".to_string();
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
        let test_file = "/test-opfs-write.txt";
        let content = "Hello, OPFS!";

        // Try to write to a file
        let result = tokio_fs_ext::write(test_file, content).await;

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
