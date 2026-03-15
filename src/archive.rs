//! Hashing, integrity verification, and tar.gz archive creation.
//!
//! This module contains pure functions — no state, no I/O.

use anyhow::{Context, Result};
use data_encoding::BASE64;
use md5::{Digest, Md5};
use sha1::Sha1;
use sha2::Sha512;

use crate::error::VerifyResult;

#[cfg(target_arch = "wasm32")]
use web_time::{SystemTime, UNIX_EPOCH};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Calculate MD5 hash of byte content (hex-encoded).
pub fn sig_md5(content: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Verify file integrity.
///
/// Returns [`VerifyResult::Verified`] if a hash was present and matched,
/// [`VerifyResult::Failed`] if present but wrong, or
/// [`VerifyResult::NoHashAvailable`] if neither `integrity` nor `shasum`
/// was provided.
///
/// `integrity` takes priority over `shasum` when both are present.
pub fn verify_integrity(
    file_bytes: &[u8],
    integrity: Option<&str>,
    shasum: Option<&str>,
) -> VerifyResult {
    if let Some(integrity_str) = integrity
        && let Some(hash_part) = integrity_str.strip_prefix("sha512-")
    {
        let mut hasher = Sha512::new();
        hasher.update(file_bytes);
        let calculated = BASE64.encode(&hasher.finalize());
        return if calculated == hash_part {
            VerifyResult::Verified
        } else {
            VerifyResult::Failed
        };
    }

    if let Some(expected) = shasum {
        let mut hasher = Sha1::new();
        hasher.update(file_bytes);
        let calculated = format!("{:x}", hasher.finalize());
        return if calculated == expected {
            VerifyResult::Verified
        } else {
            VerifyResult::Failed
        };
    }

    VerifyResult::NoHashAvailable
}

/// A single file entry for creating archives.
#[derive(Debug, Clone)]
pub struct PackFile {
    pub path: std::path::PathBuf,
    pub content: Vec<u8>,
}

impl PackFile {
    pub fn new(path: impl Into<std::path::PathBuf>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content,
        }
    }
}

/// Create a tar.gz archive from file entries.
pub fn gzip(files: &[PackFile]) -> Result<Vec<u8>> {
    use flate2::{Compression, GzBuilder};
    use tar::Builder;

    let buffer = Vec::new();
    let encoder = GzBuilder::new().write(buffer, Compression::default());
    let mut archive = Builder::new(encoder);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for file in files {
        let mut header = tar::Header::new_ustar();
        header
            .set_path(&file.path)
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

    let encoder = archive
        .into_inner()
        .context("Failed to finalize tar archive")?;
    let buffer = encoder
        .finish()
        .context("Failed to finish gzip compression")?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_sig_md5() {
        assert_eq!(
            sig_md5(b"Hello, World!"),
            "65a8e27d8879283831b664bd8b7f0ad4"
        );
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_sha512() {
        let data = b"hello world";
        let good = "sha512-MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==";
        assert!(verify_integrity(data, Some(good), None).is_verified());
        assert!(verify_integrity(data, Some("sha512-bad"), None).is_failed());
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_sha1() {
        let data = b"hello world";
        let good = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
        assert!(verify_integrity(data, None, Some(good)).is_verified());
        assert!(verify_integrity(data, None, Some("bad")).is_failed());
    }

    #[wasm_bindgen_test]
    fn test_verify_integrity_none() {
        assert_eq!(
            verify_integrity(b"x", None, None),
            VerifyResult::NoHashAvailable
        );
    }

    #[wasm_bindgen_test]
    fn test_gzip_roundtrip() {
        let files = vec![
            PackFile::new("a.txt", b"hello".to_vec()),
            PackFile::new("d/b.txt", b"world".to_vec()),
        ];
        let bytes = gzip(&files).unwrap();
        assert!(!bytes.is_empty());
    }
}
