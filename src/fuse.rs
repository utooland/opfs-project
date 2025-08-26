use crate::util::get_package_name;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;

/// Read file content as bytes (without fuse.link support)
pub(crate) async fn read_without_fuse_link(path: &str) -> Result<Vec<u8>> {
    let prepared_path = crate::util::prepare_path(path);
    let content = tokio_fs_ext::read(&prepared_path).await?;
    Ok(content)
}

/// Create fuse link between source and destination directories
pub async fn fuse_link<S: AsRef<Path>, D: AsRef<Path>>(src: S, dst: D) -> Result<()> {
    let src_ref = src.as_ref();
    let dst_ref = dst.as_ref();

    // Create the destination directory if it doesn't exist
    tokio_fs_ext::create_dir_all(dst_ref.to_string_lossy().as_ref()).await?;

    let link_file_path = dst_ref.join("fuse.link");

    // Check if fuse.link already exists
    if tokio_fs_ext::metadata(&link_file_path).await.is_ok() {
        // Read existing content
        let existing_content = tokio_fs_ext::read_to_string(&link_file_path).await?;
        let mut links: Vec<String> = existing_content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|s| s.to_string())
            .collect();

        // Add new link if not already present
        if !links.contains(&src_ref.to_string_lossy().to_string()) {
            links.push(src_ref.to_string_lossy().to_string());
        }

        // Write back all links
        let new_content = links.join("\n") + "\n";
        tokio_fs_ext::write(&link_file_path, new_content.as_bytes()).await?;
    } else {
        // Create new fuse.link file with the source path
        let link_content = format!("{}\n", src_ref.to_string_lossy());
        tokio_fs_ext::write(&link_file_path, link_content.as_bytes()).await?;
    }

    Ok(())
}

/// Get fuse.link path for a given path that contains node_modules
fn get_fuse_link_path<P: AsRef<Path>>(path: P) -> Option<std::path::PathBuf> {
    let path_ref = path.as_ref();
    let path_str = path_ref.to_string_lossy();
    if let Some(package_name) = get_package_name(&path_str) {
        // Find node_modules in the path
        if let Some(node_modules_pos) = path_str.find("node_modules") {
            // Construct the path: original_path_up_to_node_modules/node_modules/package_name/fuse.link
            let before_node_modules = &path_str[..node_modules_pos];
            let fuse_link_path = Path::new(before_node_modules)
                .join("node_modules")
                .join(package_name)
                .join("fuse.link");
            return Some(fuse_link_path);
        }
    }
    None
}

/// Try to read file through fuse link logic for node_modules
pub(super) async fn try_read_through_fuse_link<P: AsRef<Path>>(
    prepared_path: P,
) -> Result<Option<Vec<u8>>> {
    let path_ref = prepared_path.as_ref();
    let path_str = path_ref.to_string_lossy();

    if !path_str.contains("node_modules") {
        return Ok(None);
    }

    let fuse_link_path = match get_fuse_link_path(path_ref) {
        Some(path) => path,
        None => return Ok(None),
    };

    let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
        Ok(content) => content,
        Err(_) => return Ok(None),
    };

    let target_dir = link_content.lines().next().unwrap_or("").trim();
    if target_dir.is_empty() {
        return Ok(None);
    }

    let relative_path = extract_relative_path_from_node_modules(path_ref)?;
    let target_path = Path::new(target_dir).join(relative_path);

    match tokio_fs_ext::read(&target_path).await {
        Ok(content) => Ok(Some(content)),
        Err(_) => Ok(None),
    }
}

/// Extract relative path from node_modules path
fn extract_relative_path_from_node_modules<P: AsRef<Path>>(prepared_path: P) -> Result<String> {
    let path_ref = prepared_path.as_ref();
    let path_str = path_ref.to_string_lossy();

    let package_name = get_package_name(&path_str)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Could not extract package name"))?;

    let node_modules_pos = path_str.find("node_modules").ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "Could not find node_modules in path",
        )
    })?;

    // Get the part after node_modules/package_name/
    let after_package = &path_str[node_modules_pos + "node_modules".len()..];
    let after_package = after_package.trim_start_matches('/');

    // Remove the package name from the path
    let relative_path = if after_package.starts_with(&package_name) {
        after_package[package_name.len()..]
            .trim_start_matches('/')
            .to_string()
    } else {
        // Fallback: just get the filename
        path_ref
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    };

    Ok(relative_path)
}

/// Try to read directory through fuse.link for node_modules
pub(super) async fn try_read_dir_through_fuse_link<P: AsRef<Path>>(
    prepared_path: P,
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    let path_ref = prepared_path.as_ref();
    let path_str = path_ref.to_string_lossy();

    if !path_str.contains("node_modules") {
        return Ok(None);
    }

    let fuse_link_path = match get_fuse_link_path(path_ref) {
        Some(path) => path,
        None => return Ok(None),
    };

    let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
        Ok(content) => content,
        Err(_) => return Ok(None),
    };

    let target_dir = link_content.lines().next().unwrap_or("").trim();
    if target_dir.is_empty() {
        return Ok(None);
    }

    let target_path = get_target_path_for_node_modules(path_ref, target_dir)?;
    let entries = crate::util::read_dir_direct(&target_path).await?;
    Ok(Some(entries))
}

/// Get target path for node_modules directory
fn get_target_path_for_node_modules<P: AsRef<Path>, T: AsRef<Path>>(
    prepared_path: P,
    target_dir: T,
) -> Result<std::path::PathBuf> {
    let path_ref = prepared_path.as_ref();
    let path_str = path_ref.to_string_lossy();
    let target_dir_ref = target_dir.as_ref();

    let package_name = get_package_name(&path_str)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Could not extract package name"))?;

    let dir_name = path_ref
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Could not get directory name"))?;

    let dir_name_str = dir_name.to_string_lossy();

    // Check if directory name matches package name
    if dir_name_str == package_name {
        // Case 1: directory name matches package name (e.g., node_modules/lodash)
        Ok(target_dir_ref.to_path_buf())
    } else {
        // Case 2: directory name doesn't match package name (e.g., node_modules/@lodash/has)
        Ok(target_dir_ref.join(dir_name_str.as_ref()))
    }
}

/// Try to read directory through single fuse.link file
pub(super) async fn try_read_dir_through_single_fuse_link<P: AsRef<Path>>(
    prepared_path: P,
    entries: &[tokio_fs_ext::DirEntry],
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    // Check if entries only contains one entry and it is fuse.link
    if entries.len() != 1 || entries[0].file_name().to_string_lossy() != "fuse.link" {
        return Ok(None);
    }

    let path_ref = prepared_path.as_ref();
    let link_file_path = path_ref.join("fuse.link");
    let link_content = tokio_fs_ext::read_to_string(&link_file_path).await?;
    let target_dir = link_content.lines().next().unwrap_or("").trim();

    if target_dir.is_empty() {
        return Ok(None);
    }

    let entries = crate::util::read_dir_direct(target_dir).await?;
    Ok(Some(entries))
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;

    use wasm_bindgen_test::*;

    /// Test helper: create a temporary directory with test files
    async fn create_test_dir(name: &str) -> String {
        let temp_path = format!("/test-fuse-dir-{}", name);
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create test files
        tokio_fs_ext::write(&format!("{}/test.txt", temp_path), b"Hello, World!")
            .await
            .unwrap();

        tokio_fs_ext::write(
            &format!("{}/package.json", temp_path),
            b"{\"name\": \"test-package\"}",
        )
        .await
        .unwrap();

        temp_path
    }

    /// Test helper: create a node_modules structure
    async fn create_node_modules_structure(base_path: &str, package_name: &str) -> String {
        let node_modules_path = Path::new(base_path).join("node_modules");
        let package_path = node_modules_path.join(package_name);

        tokio_fs_ext::create_dir_all(&package_path).await.unwrap();

        // Create fuse.link file
        let fuse_link_content = format!("{}\n", base_path);
        tokio_fs_ext::write(
            &package_path.join("fuse.link"),
            fuse_link_content.as_bytes(),
        )
        .await
        .unwrap();

        package_path.to_string_lossy().to_string()
    }

    #[wasm_bindgen_test]
    async fn test_fuse_link_creation() {
        let src_path = "/tmp/source";
        let dst_path = "/test-destination".to_string();
        tokio_fs_ext::create_dir_all(&dst_path).await.unwrap();

        // Create source directory
        tokio_fs_ext::create_dir_all(src_path).await.unwrap();
        tokio_fs_ext::write(
            &format!("{}/test.txt", src_path),
            "Source content".as_bytes(),
        )
        .await
        .unwrap();

        // Create fuse link
        let result = fuse_link(src_path, &dst_path).await;
        assert!(result.is_ok());

        // Verify fuse.link file was created
        let link_file_path = Path::new(&dst_path).join("fuse.link");
        let content = crate::read(&link_file_path).await.unwrap();
        let link_content = String::from_utf8(content).unwrap();
        assert_eq!(link_content.trim(), src_path);
    }

    #[wasm_bindgen_test]
    async fn test_fuse_link_multiple_sources() {
        let dst_path = "/test-destination-multi".to_string();
        tokio_fs_ext::create_dir_all(&dst_path).await.unwrap();

        // Create first fuse link
        fuse_link("/tmp/source1", &dst_path).await.unwrap();

        // Add second source
        let result = fuse_link("/tmp/source2", &dst_path).await;
        assert!(result.is_ok());

        // Verify both sources are in fuse.link
        let link_file_path = Path::new(&dst_path).join("fuse.link");
        let content = crate::read(&link_file_path).await.unwrap();
        let link_content = String::from_utf8(content).unwrap();
        let lines: Vec<&str> = link_content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.contains(&"/tmp/source1"));
        assert!(lines.contains(&"/tmp/source2"));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path() {
        let test_cases = vec![
            (
                "/path/to/node_modules/lodash/index.js",
                Some("/path/to/node_modules/lodash/fuse.link"),
            ),
            (
                "/path/to/node_modules/@types/node/index.d.ts",
                Some("/path/to/node_modules/@types/node/fuse.link"),
            ),
            ("/path/to/some/file.txt", None),
            ("/path/to/node_modules/", None),
        ];

        for (input, expected) in test_cases {
            let result = get_fuse_link_path(input);
            // Normalize path separators for comparison
            let normalized_result = result.map(|p| p.to_string_lossy().replace("//", "/"));
            let normalized_expected = expected.map(|s| s.to_string().replace("//", "/"));
            assert_eq!(normalized_result, normalized_expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_extract_relative_path_from_node_modules() {
        let test_cases = vec![
            ("/path/to/node_modules/lodash/index.js", "index.js"),
            ("/path/to/node_modules/@types/node/index.d.ts", "index.d.ts"),
            ("/path/to/node_modules/lodash/lib/array.js", "lib/array.js"),
            (
                "/path/to/node_modules/@types/node/types/index.d.ts",
                "types/index.d.ts",
            ),
        ];

        for (input, expected) in test_cases {
            let result = extract_relative_path_from_node_modules(input).unwrap();
            assert_eq!(result, expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_extract_relative_path_from_node_modules_invalid() {
        let invalid_paths = vec![
            "/path/to/some/file.txt",
            "/path/to/node_modules/",
            "/path/to/node_modules",
        ];

        for path in invalid_paths {
            let result = extract_relative_path_from_node_modules(path);
            assert!(result.is_err());
        }
    }

    #[wasm_bindgen_test]
    async fn test_read_through_fuse_link() {
        let base_path = create_test_dir("test-read-through-fuse-link").await;
        let package_name = "test-package";
        let package_path = create_node_modules_structure(&base_path, package_name).await;

        // Create a file in the source directory
        let source_file = Path::new(&base_path).join("source_file.txt");
        tokio_fs_ext::write(&source_file, b"Fuse link content")
            .await
            .unwrap();

        // Test reading through fuse link
        let node_modules_file = Path::new(&package_path).join("source_file.txt");
        let result = try_read_through_fuse_link(&node_modules_file)
            .await
            .unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"Fuse link content");
    }

    #[wasm_bindgen_test]
    async fn test_read_through_fuse_link_no_node_modules() {
        let result = try_read_through_fuse_link("/path/to/regular/file.txt")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[wasm_bindgen_test]
    async fn test_read_through_fuse_link_no_fuse_link_file() {
        let result = try_read_through_fuse_link("/path/to/node_modules/lodash/index.js")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_through_fuse_link() {
        let base_path = create_test_dir("test-read-dir-through-fuse-link").await;
        let package_name = "test-package";
        let package_path = create_node_modules_structure(&base_path, package_name).await;

        // Create a subdirectory in the source
        let source_subdir = Path::new(&base_path).join("subdir");
        tokio_fs_ext::create_dir_all(&source_subdir).await.unwrap();
        tokio_fs_ext::write(&source_subdir.join("subfile.txt"), b"Subdirectory file")
            .await
            .unwrap();

        // Test reading directory through fuse link
        let result = try_read_dir_through_fuse_link(&package_path).await.unwrap();

        assert!(result.is_some());
        let entries = result.unwrap();
        assert!(!entries.is_empty());

        // Verify we can find the test files
        let file_names: Vec<String> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(file_names.contains(&"test.txt".to_string()));
        assert!(file_names.contains(&"package.json".to_string()));
        assert!(file_names.contains(&"subdir".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_get_target_path_for_node_modules() {
        let test_cases = vec![
            ("/path/to/node_modules/lodash", "/tmp/source", "/tmp/source"),
            (
                "/path/to/node_modules/@types/node",
                "/tmp/source",
                "/tmp/source/node",
            ),
            (
                "/path/to/node_modules/lodash/lib",
                "/tmp/source",
                "/tmp/source/lib",
            ),
        ];

        for (prepared_path, target_dir, expected) in test_cases {
            let result = get_target_path_for_node_modules(prepared_path, target_dir).unwrap();
            // Normalize path separators for comparison
            let normalized_result = result.to_string_lossy().replace("//", "/");
            let normalized_expected = expected.replace("//", "/");
            assert_eq!(normalized_result, normalized_expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_read_with_fuse_link() {
        let base_path = create_test_dir("test-read-with-fuse-link").await;
        let package_name = "test-package";
        let package_path = create_node_modules_structure(&base_path, package_name).await;

        // Create a file in the source directory
        let source_file = Path::new(&base_path).join("fuse_test.txt");
        tokio_fs_ext::write(&source_file, b"Fuse link test content")
            .await
            .unwrap();

        // Test reading through the main read function
        let node_modules_file = Path::new(&package_path).join("fuse_test.txt");
        let result = crate::read(&node_modules_file).await.unwrap();

        assert_eq!(result, b"Fuse link test content");
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_with_fuse_link() {
        let base_path = create_test_dir("test-read-dir-with-fuse-link").await;
        let package_name = "test-package";
        let package_path = create_node_modules_structure(&base_path, package_name).await;

        // Test reading directory through the main read_dir function
        let result = crate::read_dir(&package_path).await.unwrap();

        assert!(!result.is_empty());
        let file_names: Vec<String> = result
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(file_names.contains(&"test.txt".to_string()));
        assert!(file_names.contains(&"package.json".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_read_regular_file() {
        let base_path = create_test_dir("test-read-regular-file").await;

        // Test reading a regular file (not in node_modules)
        let result = tokio_fs_ext::read(&Path::new(&base_path).join("test.txt"))
            .await
            .unwrap();
        assert_eq!(result, b"Hello, World!");
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_regular_directory() {
        let base_path = create_test_dir("test-read-dir-regular-directory").await;

        // Test reading a regular directory

        let result = crate::read_dir(&base_path).await.unwrap();

        assert!(!result.is_empty());
        let file_names: Vec<String> = result
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(file_names.contains(&"test.txt".to_string()));
        assert!(file_names.contains(&"package.json".to_string()));
    }
}
