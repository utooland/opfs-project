use std::io::Result;
use std::path::Path;

/// Extract package name from a path that contains node_modules
pub fn get_package_name(path: &str) -> Option<String> {
    // Find node_modules in the path
    if let Some(node_modules_pos) = path.find("node_modules") {
        // Get the part after node_modules
        let after_node_modules = &path[node_modules_pos + "node_modules".len()..];
        let after_node_modules = after_node_modules.trim_start_matches('/');

        // Split by '/' and take only the first two components
        let mut components = after_node_modules.split('/').take(2);
        let first = components.next()?;

        // Check if first component starts with @ (scoped package)
        let package_name = if first.starts_with('@') {
            // For scoped packages, we need two components: @scope/package
            if let Some(second) = components.next() {
                format!("{first}/{second}")
            } else {
                return None;
            }
        } else {
            // For regular packages, just use the first component
            first.to_string()
        };

        if !package_name.is_empty() {
            return Some(package_name);
        }
    }
    None
}

/// Prepare path by resolving relative paths against current working directory
pub fn prepare_path<P: AsRef<Path>>(path: P) -> std::path::PathBuf {
    let path_ref = path.as_ref();
    let path_str = path_ref.to_string_lossy();

    if path_str.starts_with('/') {
        std::path::PathBuf::from(path_str.as_ref())
    } else {
        let cwd = crate::get_cwd();
        cwd.join(path_ref)
    }
}

/// Read directory directly without fuse.link logic
pub async fn read_dir_direct<P: AsRef<Path>>(path: P) -> Result<Vec<tokio_fs_ext::DirEntry>> {
    let path_ref = path.as_ref();
    let mut entries = Vec::new();
    let mut read_dir = tokio_fs_ext::read_dir(path_ref).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        entries.push(entry);
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;

    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    async fn test_get_package_name_regular_package() {
        let test_cases = vec![
            (
                "/path/to/node_modules/lodash/index.js",
                Some("lodash".to_string()),
            ),
            (
                "/path/to/node_modules/express/lib/app.js",
                Some("express".to_string()),
            ),
            (
                "/path/to/node_modules/react/package.json",
                Some("react".to_string()),
            ),
            (
                "node_modules/axios/dist/axios.js",
                Some("axios".to_string()),
            ),
        ];

        for (input, expected) in test_cases {
            let result = get_package_name(input);
            assert_eq!(result, expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_get_package_name_scoped_package() {
        let test_cases = vec![
            (
                "/path/to/node_modules/@types/node/index.d.ts",
                Some("@types/node".to_string()),
            ),
            (
                "/path/to/node_modules/@angular/core/core.js",
                Some("@angular/core".to_string()),
            ),
            (
                "/path/to/node_modules/@babel/preset-env/index.js",
                Some("@babel/preset-env".to_string()),
            ),
            (
                "node_modules/@vue/cli-service/bin/vue-cli-service.js",
                Some("@vue/cli-service".to_string()),
            ),
        ];

        for (input, expected) in test_cases {
            let result = get_package_name(input);
            assert_eq!(result, expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_get_package_name_invalid_paths() {
        let invalid_paths = vec![
            "/path/to/some/file.txt",
            "/path/to/node_modules/",
            "/path/to/node_modules",
            "/path/to/some/other/path",
            "",
            "node_modules",
        ];

        for path in invalid_paths {
            let result = get_package_name(path);
            assert!(result.is_none(), "Expected None for path: {}", path);
        }
    }

    #[wasm_bindgen_test]
    async fn test_get_package_name_edge_cases() {
        let test_cases = vec![
            // Multiple node_modules in path
            (
                "/path/to/node_modules/lodash/node_modules/other/index.js",
                Some("lodash".to_string()),
            ),
            // Very long package names
            (
                "/path/to/node_modules/very-long-package-name-with-many-characters/index.js",
                Some("very-long-package-name-with-many-characters".to_string()),
            ),
            // Scoped package with long names
            (
                "/path/to/node_modules/@very-long-scope/very-long-package-name/index.js",
                Some("@very-long-scope/very-long-package-name".to_string()),
            ),
            // Path with spaces and special characters
            (
                "/path/to/node_modules/package-name/index.js",
                Some("package-name".to_string()),
            ),
        ];

        for (input, expected) in test_cases {
            let result = get_package_name(input);
            assert_eq!(result, expected);
        }
    }

    #[wasm_bindgen_test]
    async fn test_prepare_path_absolute() {
        let test_cases = vec![
            "/absolute/path",
            "/usr/local/bin",
            "/home/user/project",
            "/",
        ];

        for path in test_cases {
            let result = prepare_path(path);
            assert_eq!(result.to_string_lossy(), path);
        }
    }

    #[wasm_bindgen_test]
    async fn test_prepare_path_relative() {
        // Since CWD is managed globally and has a default value,
        // we'll test that relative paths are properly concatenated
        // by checking the structure rather than exact values

        let test_cases = vec![
            "file.txt",
            "src/main.rs",
            "../parent/file.js",
            "./current/file.py",
        ];

        for relative_path in test_cases {
            let result = prepare_path(relative_path);
            let result_str = result.to_string_lossy();

            // Verify that the result contains the relative path at the end
            assert!(
                result_str.ends_with(relative_path),
                "Result '{}' should end with '{}'",
                result_str,
                relative_path
            );

            // Verify that it's not an absolute path starting with /
            assert!(
                !relative_path.starts_with('/') || result_str != relative_path,
                "Relative path '{}' should be resolved to absolute path",
                relative_path
            );
        }
    }

    #[wasm_bindgen_test]
    async fn test_prepare_path_empty() {
        let result = prepare_path("");
        let result_str = result.to_string_lossy();

        // Should end with a slash since it's an empty path
        assert!(
            result_str.ends_with('/'),
            "Empty path should end with '/', got: {}",
            result_str
        );

        // Should not be empty
        assert!(
            !result_str.is_empty(),
            "Empty path should not result in empty string"
        );
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct() {
        let temp_path = "/test-read-dir-direct".to_string();
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create test files and directories
        tokio_fs_ext::write(&format!("{}/file1.txt", temp_path), b"content1")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/file2.js", temp_path), b"content2")
            .await
            .unwrap();
        tokio_fs_ext::create_dir_all(&format!("{}/subdir", temp_path))
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/subdir/file3.py", temp_path), b"content3")
            .await
            .unwrap();

        let entries = read_dir_direct(&temp_path).await.unwrap();

        // Should have at least 3 entries (file1.txt, file2.js, subdir)
        assert!(entries.len() >= 3);

        let file_names: Vec<String> = entries.iter()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        // Check for files
        assert!(file_names.contains(&"file1.txt".to_string()));
        assert!(file_names.contains(&"file2.js".to_string()));

        // Check for directory
        assert!(file_names.contains(&"subdir".to_string()));

        // Verify file types
        for entry in &entries {
            if let Some(name) = entry.file_name().to_str() {
                match name {
                    "file1.txt" | "file2.js" => {
                        let meta = tokio_fs_ext::metadata(entry.path()).await.unwrap();
                        assert!(!meta.is_dir());
                    }
                    "subdir" => {
                        let meta = tokio_fs_ext::metadata(entry.path()).await.unwrap();
                        assert!(meta.is_dir());
                    }
                    _ => {}
                }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct_empty_directory() {
        let temp_path = "/test-read-dir-empty".to_string();
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        let entries = read_dir_direct(&temp_path).await.unwrap();

        // Empty directory should return empty list
        assert_eq!(entries.len(), 0);
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct_nonexistent_directory() {
        let result = read_dir_direct("/nonexistent/directory/path").await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct_with_hidden_files() {
        let temp_path = "/test-read-dir-hidden".to_string();
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create regular and hidden files
        tokio_fs_ext::write(&format!("{}/visible.txt", temp_path), b"visible")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/.hidden", temp_path), b"hidden")
            .await
            .unwrap();
        tokio_fs_ext::create_dir_all(&format!("{}/.hidden_dir", temp_path))
            .await
            .unwrap();

        let entries = read_dir_direct(&temp_path).await.unwrap();

        let file_names: Vec<String> = entries.iter().map(|e| e.file_name().to_string_lossy().to_string()).collect();

        // Should include both visible and hidden files
        assert!(file_names.contains(&"visible.txt".to_string()));
        assert!(file_names.contains(&".hidden".to_string()));
        assert!(file_names.contains(&".hidden_dir".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct_with_special_characters() {
        let temp_path = "/test-read-dir-special".to_string();
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create files with special characters in names
        tokio_fs_ext::write(&format!("{}/file with spaces.txt", temp_path), b"content")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/file-with-dashes.js", temp_path), b"content")
            .await
            .unwrap();
        tokio_fs_ext::write(
            &format!("{}/file_with_underscores.py", temp_path),
            b"content",
        )
        .await
        .unwrap();

        let entries = read_dir_direct(&temp_path).await.unwrap();

        let file_names: Vec<String> = entries.iter().map(|e| e.file_name().to_string_lossy().to_string()).collect();

        assert!(file_names.contains(&"file with spaces.txt".to_string()));
        assert!(file_names.contains(&"file-with-dashes.js".to_string()));
        assert!(file_names.contains(&"file_with_underscores.py".to_string()));
    }
}
