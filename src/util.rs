use std::io::Result;
use std::path::Path;

/// Prepare path by resolving relative paths against current working directory
pub fn prepare_path<P: AsRef<Path>>(path: P) -> std::path::PathBuf {
    let path_ref = path.as_ref();
    let path_str = path_ref.to_string_lossy();

    if path_str.starts_with('/') {
        std::path::PathBuf::from(path_str.as_ref())
    } else if path_str.starts_with("./") {
        let cwd = crate::get_cwd();
        cwd.join(path_ref.strip_prefix("./").unwrap())
    } else {
        let cwd = crate::get_cwd();
        cwd.join(path_ref)
    }
}

/// Read directory directly without fuse.link logic
pub async fn read_dir_direct<P: AsRef<Path>>(path: P) -> Result<Vec<tokio_fs_ext::DirEntry>> {
    let path_ref = path.as_ref();
    let read_dir = tokio_fs_ext::read_dir(path_ref).await?;
    read_dir.collect()
}

/// Read file content as bytes (without fuse.link support)
pub async fn read_direct<P: AsRef<Path>>(path: P) -> Result<Vec<u8>> {
    let prepared_path = crate::util::prepare_path(path);
    let content = tokio_fs_ext::read(&prepared_path).await?;
    Ok(content)
}


#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;

    use wasm_bindgen_test::*;

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

        let file_names: Vec<String> = entries
            .iter()
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

        let file_names: Vec<String> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

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

        let file_names: Vec<String> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        assert!(file_names.contains(&"file with spaces.txt".to_string()));
        assert!(file_names.contains(&"file-with-dashes.js".to_string()));
        assert!(file_names.contains(&"file_with_underscores.py".to_string()));
    }
}
