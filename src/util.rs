use std::io::Result;
use std::path::Path;

/// Prepare path by resolving relative paths against current working directory
pub fn prepare_path<P: AsRef<Path>>(path: P) -> std::path::PathBuf {
    let path_ref = path.as_ref();
    if path_ref.starts_with("/") {
        path_ref.to_path_buf()
    } else if let Ok(stripped) = path_ref.strip_prefix(".") {
        crate::get_cwd().join(stripped)
    } else {
        crate::get_cwd().join(path_ref)
    }
}

/// Read directory directly without fuse.link logic
pub async fn read_dir_direct<P: AsRef<Path>>(path: P) -> Result<Vec<tokio_fs_ext::DirEntry>> {
    tokio_fs_ext::read_dir(path.as_ref()).await?.collect()
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_prepare_path_absolute() {
        assert_eq!(prepare_path("/absolute/path").to_string_lossy(), "/absolute/path");
    }

    #[wasm_bindgen_test]
    fn test_prepare_path_relative() {
        let result = prepare_path("file.txt");
        assert!(result.to_string_lossy().ends_with("file.txt"));
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct() {
        let temp_path = "/test-read-dir";
        tokio_fs_ext::create_dir_all(temp_path).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file.txt", temp_path), b"content").await.unwrap();

        let entries = read_dir_direct(temp_path).await.unwrap();
        assert!(!entries.is_empty());
    }

    #[wasm_bindgen_test]
    async fn test_read_dir_direct_nonexistent() {
        assert!(read_dir_direct("/nonexistent").await.is_err());
    }
}
