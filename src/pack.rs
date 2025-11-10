use anyhow::{Context, Result};
use std::path::Path;
use md5::{Md5, Digest};
use sha1::{Sha1, Digest as Sha1Digest};
use sha2::{Sha512, Digest as Sha512Digest};
use data_encoding::BASE64;

/// Generate MD5 signature for a file path
pub async fn sign<P: AsRef<Path>>(path: P) -> Result<String> {
    let path_ref = path.as_ref();
    let content = tokio_fs_ext::read(path_ref)
        .await
        .context(format!("Failed to read file: {}", path_ref.display()))?;

    let mut hasher = Md5::new();
    hasher.update(&content);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Generate MD5 signature for byte content
pub fn sign_bytes(content: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Verify file integrity using shasum or integrity field
/// Returns true if the file matches the expected hash, false otherwise
///
/// Supports two verification methods:
/// - `integrity`: SHA512 hash in base64 format (e.g., "sha512-...")
/// - `shasum`: SHA1 hash in hex format
pub fn verify_integrity(file_bytes: &[u8], integrity: Option<&str>, shasum: Option<&str>) -> bool {
    // Try integrity first (sha512)
    if let Some(integrity_str) = integrity {
        if let Some(hash_part) = integrity_str.strip_prefix("sha512-") {
            let mut hasher = Sha512::new();
            hasher.update(file_bytes);
            let result = hasher.finalize();
            let calculated = BASE64.encode(&result);
            return calculated == hash_part;
        }
    }

    // Fall back to shasum (sha1)
    if let Some(expected_shasum) = shasum {
        let mut hasher = Sha1::new();
        hasher.update(file_bytes);
        let result = hasher.finalize();
        let calculated = format!("{:x}", result);
        return calculated == expected_shasum;
    }

    // If no hash information is available, we can't verify
    false
}

/// Create a tar.gz archive from source directory to destination file
///
/// This function performs I/O operations in a blocking context to avoid
/// blocking the async runtime. The actual tar creation happens in a
/// separate thread pool.
pub async fn zip<S: AsRef<Path>, D: AsRef<Path>>(src: S, dist: D) -> Result<()> {
    let src_path = src.as_ref().to_path_buf();
    let dist_path = dist.as_ref().to_path_buf();

    // Create destination directory if it doesn't exist (async)
    if let Some(parent) = dist_path.parent() {
        tokio_fs_ext::create_dir_all(parent)
            .await
            .context(format!("Failed to create parent directory: {}", parent.display()))?;
    }

    // Perform blocking I/O operations in a separate thread
    tokio::task::spawn_blocking(move || {
        create_tar_gz(&src_path, &dist_path)
    })
    .await
    .context("Failed to spawn blocking task")?
    .context(format!(
        "Failed to create archive from {} to {}",
        src_path.display(),
        dist_path.display()
    ))
}

/// Create tar.gz archive (blocking operation)
fn create_tar_gz(src: &Path, dist: &Path) -> Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::Builder;
    use std::fs::File;

    // Create tar.gz file
    let file = File::create(dist)
        .context(format!("Failed to create archive file: {}", dist.display()))?;

    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(enc);

    // Add source directory to archive
    tar.append_dir_all(".", src)
        .context(format!("Failed to add directory to archive: {}", src.display()))?;

    // Finish writing
    let encoder = tar.into_inner()
        .context("Failed to finalize tar archive")?;

    encoder.finish()
        .context("Failed to finish gzip compression")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use wasm_bindgen_test::*;
    use super::*;

    #[wasm_bindgen_test]
    async fn test_sign_function() {
        let temp_path = "/test-sign-function";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create a test file
        let test_file = format!("{}/test.txt", temp_path);
        let content = b"Hello, World!";
        tokio_fs_ext::write(&test_file, content).await.unwrap();

        // Get MD5 signature
        let signature = sign(&test_file).await.unwrap();

        // Expected MD5 for "Hello, World!" is 65a8e27d8879283831b664bd8b7f0ad4
        assert_eq!(signature, "65a8e27d8879283831b664bd8b7f0ad4");
    }

    #[wasm_bindgen_test]
    fn test_sign_bytes() {
        let content = b"Hello, World!";
        let signature = sign_bytes(content);
        assert_eq!(signature, "65a8e27d8879283831b664bd8b7f0ad4");
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_with_sha512() {
        // Test data: "hello world"
        let test_data = b"hello world";

        // SHA512 hash of "hello world" in base64
        let expected_integrity = "sha512-MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==";

        // Verify with correct integrity
        assert!(verify_integrity(test_data, Some(expected_integrity), None));

        // Verify with incorrect integrity
        assert!(!verify_integrity(test_data, Some("sha512-incorrect"), None));
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_with_shasum() {
        // Test data: "hello world"
        let test_data = b"hello world";

        // SHA1 hash of "hello world" is 2aae6c35c94fcfb415dbe95f408b9ce91ee846ed
        let expected_shasum = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

        // Verify with correct shasum
        assert!(verify_integrity(test_data, None, Some(expected_shasum)));

        // Verify with incorrect shasum
        assert!(!verify_integrity(test_data, None, Some("incorrect_hash")));
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_priority() {
        // Test data: "hello world"
        let test_data = b"hello world";

        let correct_integrity = "sha512-MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==";
        let correct_shasum = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

        // When both are provided, integrity should take priority
        // If integrity is correct, should return true even if shasum is wrong
        assert!(verify_integrity(test_data, Some(correct_integrity), Some("wrong_shasum")));

        // If integrity is wrong, should return false even if shasum is correct
        assert!(!verify_integrity(test_data, Some("sha512-wrong"), Some(correct_shasum)));
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_no_hash() {
        let test_data = b"hello world";

        // If no hash information is available, should return false
        assert!(!verify_integrity(test_data, None, None));
    }

    #[wasm_bindgen_test]
    async fn test_sign_nonexistent_file() {
        let result = sign("/nonexistent/file.txt").await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_zip_function() {
        let temp_path = "/test-zip-function";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create source directory with files
        let src_dir = format!("{}/src", temp_path);
        tokio_fs_ext::create_dir_all(&src_dir).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file1.txt", src_dir), b"content1")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/file2.txt", src_dir), b"content2")
            .await
            .unwrap();

        // Create subdirectory
        let sub_dir = format!("{}/sub", src_dir);
        tokio_fs_ext::create_dir_all(&sub_dir).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file3.txt", sub_dir), b"content3")
            .await
            .unwrap();

        // Create destination path
        let dist_file = format!("{}/archive.tar.gz", temp_path);

        // Create zip archive
        let result = zip(&src_dir, &dist_file).await;
        assert!(result.is_ok(), "Failed to create archive: {:?}", result);

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok(), "Archive file should exist");

        // Verify archive is not empty
        let metadata = metadata.unwrap();
        assert!(metadata.len() > 0, "Archive should not be empty");
    }

    #[wasm_bindgen_test]
    async fn test_zip_empty_directory() {
        let temp_path = "/test-zip-empty";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create empty source directory
        let src_dir = format!("{}/empty", temp_path);
        tokio_fs_ext::create_dir_all(&src_dir).await.unwrap();

        // Create destination path
        let dist_file = format!("{}/empty.tar.gz", temp_path);

        // Create zip archive of empty directory
        let result = zip(&src_dir, &dist_file).await;
        assert!(result.is_ok());

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_zip_nested_structure() {
        let temp_path = "/test-zip-nested";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create nested directory structure
        let src_dir = format!("{}/src", temp_path);
        let deep_dir = format!("{}/a/b/c/d", src_dir);
        tokio_fs_ext::create_dir_all(&deep_dir).await.unwrap();

        // Create files at different levels
        tokio_fs_ext::write(&format!("{}/root.txt", src_dir), b"root")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/a/level1.txt", src_dir), b"level1")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/a/b/level2.txt", src_dir), b"level2")
            .await
            .unwrap();
        tokio_fs_ext::write(&format!("{}/deep.txt", deep_dir), b"deep")
            .await
            .unwrap();

        // Create archive
        let dist_file = format!("{}/nested.tar.gz", temp_path);
        let result = zip(&src_dir, &dist_file).await;
        assert!(result.is_ok());

        // Verify archive exists
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_zip_nonexistent_source() {
        let result = zip("/nonexistent/source", "/tmp/output.tar.gz").await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_zip_creates_parent_directory() {
        let temp_path = "/test-zip-parent";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create source directory
        let src_dir = format!("{}/src", temp_path);
        tokio_fs_ext::create_dir_all(&src_dir).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file.txt", src_dir), b"content")
            .await
            .unwrap();

        // Destination with non-existent parent directory
        let dist_file = format!("{}/nested/path/archive.tar.gz", temp_path);

        // Create archive - should create parent directories
        let result = zip(&src_dir, &dist_file).await;
        assert!(result.is_ok());

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok());
    }
}
