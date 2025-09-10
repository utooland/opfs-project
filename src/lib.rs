#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::{
    io::Result,
    path::{Path, PathBuf},
};

mod fuse;
mod package_lock;
pub mod package_manager;
pub mod tracing_init;
mod util;

pub use tokio_fs_ext::DirEntry;

/// Read file content with fuse.link support
pub async fn read<P: AsRef<Path>>(path: P) -> Result<Vec<u8>> {
    let path_ref = path.as_ref();
    let prepared_path = crate::util::prepare_path(path_ref);

    // Try to read through node_modules fuse link logic first
    if let Some(content) = fuse::try_read_through_fuse_link(&prepared_path).await? {
        return Ok(content);
    }

    // Fallback to direct read
    let content = tokio_fs_ext::read(&prepared_path).await?;
    Ok(content)
}

/// Read directory contents with file type information and fuse.link support
pub async fn read_dir<P: AsRef<Path>>(path: P) -> Result<Vec<tokio_fs_ext::DirEntry>> {
    let path_ref = path.as_ref();
    let prepared_path = crate::util::prepare_path(path_ref);

    // Handle node_modules fuse.link logic
    if let Some(entries) = fuse::try_read_dir_through_fuse_link(&prepared_path).await? {
        return Ok(entries);
    }

    // Handle direct directory reading
    let entries = crate::util::read_dir_direct(&prepared_path).await?;
    Ok(entries)
}

pub async fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::write(path, content).await
}

pub async fn create_dir(path: impl AsRef<Path>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::create_dir(path).await
}

pub async fn create_dir_all(path: impl AsRef<Path>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::create_dir_all(path).await
}

pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<u64> {
    // TODO: try fuse link first
    tokio_fs_ext::copy(from, to).await
}

pub async fn remove_file(path: impl AsRef<Path>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::remove_file(path).await
}

pub async fn remove_dir(path: impl AsRef<Path>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::remove_dir(path).await
}

pub async fn remove_dir_all(path: impl AsRef<Path>) -> Result<()> {
    // TODO: try fuse link first
    tokio_fs_ext::remove_dir_all(path).await
}

pub async fn metadata(path: impl AsRef<Path>) -> Result<tokio_fs_ext::Metadata> {
    // TODO: try fuse link first
    tokio_fs_ext::metadata(path).await
}

/// Set current working directory
pub fn set_cwd(path: impl AsRef<Path>) {
    tokio_fs_ext::set_current_dir(path).unwrap();
}

/// Read current working directory
pub fn get_cwd() -> PathBuf {
    tokio_fs_ext::current_dir().unwrap()
}

/// Clear the internal fuse.link cache to free memory
/// This is useful for long-running applications or testing
pub fn clear_fuse_cache() {
    fuse::clear_fuse_link_cache();
}

/// Get fuse cache statistics for monitoring
/// Returns (cache_size, list_of_cached_paths)
pub fn get_fuse_cache_stats() -> (usize, Vec<String>) {
    fuse::get_cache_stats()
}
