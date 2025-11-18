use anyhow::{Context, Result};
use md5::{Md5, Digest};
use sha1::Sha1;
use sha2::Sha512;
use data_encoding::BASE64;

#[cfg(target_arch = "wasm32")]
use web_time::{SystemTime, UNIX_EPOCH};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Calculate MD5 hash of byte content
///
/// Returns a hex-encoded MD5 hash string.
///
/// # Example
/// ```ignore
/// let hash = sig_md5(b"hello world");
/// assert_eq!(hash, "5eb63bbbe01eeed093cb22bb8f5acdc3");
/// ```
pub fn sig_md5(content: &[u8]) -> String {
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

/// File entry for creating archives
#[derive(Debug, Clone)]
pub struct PackFile {
    /// Relative path in the archive
    pub path: std::path::PathBuf,
    /// File content
    pub content: Vec<u8>,
}

impl PackFile {
    /// Create a new PackFile
    pub fn new(path: impl Into<std::path::PathBuf>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content,
        }
    }
}

/// Create tar.gz archive bytes from file entries
///
/// # Example
/// ```ignore
/// let files = vec![
///     PackFile::new("file1.txt", b"content1".to_vec()),
///     PackFile::new("dir/file2.txt", b"content2".to_vec()),
/// ];
/// let compressed_bytes = gzip(&files)?;
/// ```
pub fn gzip(files: &[PackFile]) -> Result<Vec<u8>> {
    use flate2::{Compression, GzBuilder};
    use tar::Builder;

    let buffer = Vec::new();
    let encoder = GzBuilder::new()
        .write(buffer, Compression::default());

    let mut archive = Builder::new(encoder);

    // Get current timestamp in seconds since epoch
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for file in files {
        let mut header = tar::Header::new_ustar();
        header.set_path(&file.path)
            .context(format!("Failed to set path: {}", file.path.display()))?;
        header.set_size(file.content.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(now);
        header.set_uid(1000);
        header.set_gid(1000);
        header.set_cksum();
        archive
            .append(&header, file.content.as_slice())
            .context(format!("Failed to append file: {}", file.path.display()))?;
    }

    let encoder = archive.into_inner()
        .context("Failed to finalize tar archive")?;

    let buffer = encoder.finish()
        .context("Failed to finish gzip compression")?;

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use wasm_bindgen_test::*;
    use super::*;

    #[wasm_bindgen_test]
    fn test_sig_md5() {
        let content = b"Hello, World!";
        let hash = sig_md5(content);
        assert_eq!(hash, "65a8e27d8879283831b664bd8b7f0ad4");
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
    fn test_gzip() {
        // Create file list manually
        let files = vec![
            PackFile::new("file1.txt", b"content1".to_vec()),
            PackFile::new("dir/file2.txt", b"content2".to_vec()),
            PackFile::new("README.md", b"# Test".to_vec()),
        ];

        // Create archive using gzip
        let result = gzip(&files);
        assert!(result.is_ok(), "Failed to create archive: {:?}", result);

        // Verify archive bytes are not empty
        let bytes = result.unwrap();
        assert!(bytes.len() > 0, "Archive should not be empty");
    }

    #[wasm_bindgen_test]
    fn test_gzip_empty() {
        // Create empty file list
        let files = vec![];

        // Create archive using gzip
        let result = gzip(&files);
        assert!(result.is_ok());

        // Verify archive bytes are not empty (tar.gz header should exist)
        let bytes = result.unwrap();
        assert!(bytes.len() > 0);
    }

    #[wasm_bindgen_test]
    fn test_gzip_nested_structure() {
        // Create files with nested paths
        let files = vec![
            PackFile::new("root.txt", b"root".to_vec()),
            PackFile::new("a/level1.txt", b"level1".to_vec()),
            PackFile::new("a/b/level2.txt", b"level2".to_vec()),
            PackFile::new("a/b/c/d/deep.txt", b"deep".to_vec()),
        ];

        // Create archive
        let result = gzip(&files);
        assert!(result.is_ok());

        // Verify archive exists and is not empty
        let bytes = result.unwrap();
        assert!(bytes.len() > 0);
    }
}
