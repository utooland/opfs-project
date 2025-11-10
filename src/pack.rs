use anyhow::{Context, Result};
use std::path::Path;
use md5::{Md5, Digest};
use sha1::Sha1;
use sha2::Sha512;
use data_encoding::BASE64;

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

/// Calculate MD5 hash of a file
///
/// Reads the file and returns its MD5 hash as a hex-encoded string.
///
/// # Example
/// ```ignore
/// let hash = sig_md5_file("/path/to/file.txt").await?;
/// ```
pub async fn sig_md5_file<P: AsRef<Path>>(path: P) -> Result<String> {
    let path_ref = path.as_ref();
    let content = tokio_fs_ext::read(path_ref)
        .await
        .context(format!("Failed to read file: {}", path_ref.display()))?;
    Ok(sig_md5(&content))
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

/// Create a tar.gz archive from a list of files and write to destination
///
/// This is a pure compression function that only handles creating the archive.
/// File collection is left to the caller, providing maximum flexibility.
///
/// # Example
/// ```ignore
/// let files = vec![
///     PackFile::new("src/main.rs", b"fn main() {}".to_vec()),
///     PackFile::new("Cargo.toml", b"[package]".to_vec()),
/// ];
/// gzip(files, "./archive.tar.gz").await?;
/// ```
pub async fn gzip<D: AsRef<Path>>(files: Vec<PackFile>, dest: D) -> Result<()> {
    let dest_ref = dest.as_ref();

    // Create destination directory if it doesn't exist
    if let Some(parent) = dest_ref.parent() {
        tokio_fs_ext::create_dir_all(parent)
            .await
            .context(format!("Failed to create parent directory: {}", parent.display()))?;
    }

    // Create tar.gz archive in memory
    let archive_data = create_tar_gz_bytes(files)
        .context("Failed to create tar.gz archive")?;

    // Write archive to disk
    tokio_fs_ext::write(dest_ref, &archive_data)
        .await
        .context(format!("Failed to write archive to: {}", dest_ref.display()))?;

    Ok(())
}

/// Helper function: Recursively collect all files in a directory
///
/// This is a convenience function for the common use case of archiving an entire directory.
/// For more control, use `zip_files` directly with your own file list.
pub async fn collect_dir_files(base_dir: &Path) -> Result<Vec<PackFile>> {
    let mut files = Vec::new();
    let mut stack = vec![base_dir.to_path_buf()];

    while let Some(current_dir) = stack.pop() {
        // tokio_fs_ext::read_dir returns ReadDir, need to collect to get Vec<DirEntry>
        let read_dir = tokio_fs_ext::read_dir(&current_dir)
            .await
            .context(format!("Failed to read directory: {}", current_dir.display()))?;

        let entries = read_dir.collect::<Result<Vec<_>, _>>()
            .context(format!("Failed to collect directory entries: {}", current_dir.display()))?;

        for entry in entries {
            let path = entry.path();

            // Use tokio_fs_ext::metadata instead of entry.metadata()
            // because entry.metadata() is not implemented in WASM
            let metadata = tokio_fs_ext::metadata(&path)
                .await
                .context(format!("Failed to get metadata: {}", path.display()))?;

            if metadata.is_dir() {
                stack.push(path);
            } else {
                let content = tokio_fs_ext::read(&path)
                    .await
                    .context(format!("Failed to read file: {}", path.display()))?;

                let relative_path = path
                    .strip_prefix(base_dir)
                    .context(format!("Failed to strip prefix: {}", path.display()))?
                    .to_path_buf();

                files.push(PackFile::new(relative_path, content));
            }
        }
    }

    Ok(files)
}

/// Convenience function: Create a tar.gz archive from a directory
///
/// This combines `collect_dir_files` and `gzip` for the common use case.
/// For more control over which files to include, use `gzip` directly.
///
/// # Example
/// ```ignore
/// gzip_dir("./project", "./backup.tar.gz").await?;
/// ```
pub async fn gzip_dir<S: AsRef<Path>, D: AsRef<Path>>(src: S, dest: D) -> Result<()> {
    let files = collect_dir_files(src.as_ref()).await?;
    gzip(files, dest).await
}

/// Create a tar.gz archive and return bytes (no file I/O)
///
/// This is a pure compression function that returns the compressed bytes
/// without writing to disk. Useful for in-memory operations.
///
/// # Example
/// ```ignore
/// let files = vec![
///     PackFile::new("file1.txt", b"content1".to_vec()),
///     PackFile::new("file2.txt", b"content2".to_vec()),
/// ];
/// let compressed_bytes = gzip_to_bytes(files)?;
/// ```
pub fn gzip_to_bytes(files: Vec<PackFile>) -> Result<Vec<u8>> {
    create_tar_gz_bytes(files)
}

/// Create tar.gz archive bytes from file entries
fn create_tar_gz_bytes(files: Vec<PackFile>) -> Result<Vec<u8>> {
    use flate2::{Compression, GzBuilder};
    use tar::Builder;

    let buffer = Vec::new();
    let encoder = GzBuilder::new()
        .write(buffer, Compression::default());

    let mut archive = Builder::new(encoder);

    for file in files {
        let mut header = tar::Header::new_ustar();
        header.set_path(&file.path)
            .context(format!("Failed to set path: {}", file.path.display()))?;
        header.set_size(file.content.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1700000000); // Use a fixed timestamp for reproducibility
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
    async fn test_sig_md5_file() {
        let temp_path = "/test-sig-md5-file";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create a test file
        let test_file = format!("{}/test.txt", temp_path);
        let content = b"Hello, World!";
        tokio_fs_ext::write(&test_file, content).await.unwrap();

        // Get MD5 hash
        let hash = sig_md5_file(&test_file).await.unwrap();

        // Expected MD5 for "Hello, World!" is 65a8e27d8879283831b664bd8b7f0ad4
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
    async fn test_sig_md5_file_nonexistent() {
        let result = sig_md5_file("/nonexistent/file.txt").await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_gzip() {
        let temp_path = "/test-gzip";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create file list manually
        let files = vec![
            PackFile::new("file1.txt", b"content1".to_vec()),
            PackFile::new("dir/file2.txt", b"content2".to_vec()),
            PackFile::new("README.md", b"# Test".to_vec()),
        ];

        let dist_file = format!("{}/custom.tar.gz", temp_path);

        // Create archive using gzip
        let result = gzip(files, &dist_file).await;
        assert!(result.is_ok(), "Failed to create archive: {:?}", result);

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok(), "Archive file should exist");
        assert!(metadata.unwrap().len() > 0, "Archive should not be empty");
    }

    #[wasm_bindgen_test]
    async fn test_collect_dir_files() {
        let temp_path = "/test-collect-dir-files";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create test directory structure
        let src_dir = format!("{}/src", temp_path);
        tokio_fs_ext::create_dir_all(&src_dir).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file1.txt", src_dir), b"content1")
            .await
            .unwrap();

        let sub_dir = format!("{}/sub", src_dir);
        tokio_fs_ext::create_dir_all(&sub_dir).await.unwrap();
        tokio_fs_ext::write(&format!("{}/file2.txt", sub_dir), b"content2")
            .await
            .unwrap();

        // Collect files
        let files = collect_dir_files(std::path::Path::new(&src_dir))
            .await
            .unwrap();

        // Verify collected files
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.path.to_str() == Some("file1.txt")));
        assert!(files.iter().any(|f| f.path.to_str() == Some("sub/file2.txt")));
    }

    #[wasm_bindgen_test]
    async fn test_gzip_dir() {
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

        // Create gzip archive
        let result = gzip_dir(&src_dir, &dist_file).await;
        assert!(result.is_ok(), "Failed to create archive: {:?}", result);

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok(), "Archive file should exist");

        // Verify archive is not empty
        let metadata = metadata.unwrap();
        assert!(metadata.len() > 0, "Archive should not be empty");
    }

    #[wasm_bindgen_test]
    async fn test_gzip_empty_directory() {
        let temp_path = "/test-zip-empty";
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create empty source directory
        let src_dir = format!("{}/empty", temp_path);
        tokio_fs_ext::create_dir_all(&src_dir).await.unwrap();

        // Create destination path
        let dist_file = format!("{}/empty.tar.gz", temp_path);

        // Create gzip archive of empty directory
        let result = zip(&src_dir, &dist_file).await;
        assert!(result.is_ok());

        // Verify archive was created
        let metadata = tokio_fs_ext::metadata(&dist_file).await;
        assert!(metadata.is_ok());
    }

    #[wasm_bindgen_test]
    async fn test_gzip_nested_structure() {
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
    async fn test_gzip_nonexistent_source() {
        let result = zip("/nonexistent/source", "/tmp/output.tar.gz").await;
        assert!(result.is_err());
    }

    #[wasm_bindgen_test]
    async fn test_gzip_creates_parent_directory() {
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
