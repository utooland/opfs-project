#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::{
    io::Result, panic, path::{Path, PathBuf}
};

use tracing::info;

mod fuse;
mod package_lock;
pub mod package_manager;
mod registry_fs;
pub mod pack;
mod util;

pub use tokio_fs_ext::DirEntry;

/// Read file content with fuse.link and registry support
pub async fn read<P: AsRef<Path>>(path: P) -> Result<Vec<u8>> {
    info!("Reading file: {}", path.as_ref().to_string_lossy());
    let path_ref = path.as_ref();
    let prepared_path = crate::util::prepare_path(path_ref);

    // Try to read through registry filesystem first (HTTP-based lazy loading)
    if let Some(content) = registry_fs::try_read_through_registry(&prepared_path).await? {
        return Ok(content);
    }

    // Try to read through node_modules fuse link logic
    if let Some(content) = fuse::try_read_through_fuse_link(&prepared_path).await? {
        return Ok(content);
    }

    // Fallback to direct read
    let content = tokio_fs_ext::read(&prepared_path).await?;
    Ok(content)
}

/// Read directory contents with file type information, registry, and fuse.link support
pub async fn read_dir<P: AsRef<Path>>(path: P) -> Result<Vec<tokio_fs_ext::DirEntry>> {
    let path_ref = path.as_ref();
    let prepared_path = crate::util::prepare_path(path_ref);

    // Try to read through registry filesystem first (HTTP-based lazy loading)
    if let Some(entries) = registry_fs::try_read_dir_through_registry(&prepared_path).await? {
        return Ok(entries);
    }

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

/// Initialize registry filesystem from package-lock.json
/// This enables HTTP-based lazy loading of npm packages
pub async fn init_registry_fs<P: AsRef<Path>>(lock_file_path: P) -> Result<()> {
    registry_fs::init_from_package_lock(lock_file_path).await
}

/// Clear registry filesystem caches (metadata only)
pub fn clear_registry_cache() {
    registry_fs::clear_cache()
}

/// Clear all registry filesystem caches (both metadata and file caches)
pub async fn clear_all_registry_cache() -> Result<()> {
    registry_fs::clear_all_registry_cache().await
}

/// Get registry cache statistics (for debugging)
/// Returns metadata_count
pub fn get_registry_cache_stats() -> usize {
    registry_fs::get_cache_stats()
}

#[cfg(test)]
pub mod test_utils {
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Initialize tracing-web for tests
    /// This should be called at the beginning of each test to enable web console logging
    pub fn init_tracing() {
        INIT.call_once(|| {
            {
                use tracing_subscriber::{
                    fmt::{
                        self,
                        format::FmtSpan,
                    },
                    layer::SubscriberExt,
                    registry,
                    util::SubscriberInitExt,
                };

                use tracing_web::MakeWebConsoleWriter;

                let fmt_layer = fmt::layer()
                .without_time()
                .with_span_events(FmtSpan::CLOSE)
                .with_writer(MakeWebConsoleWriter::new());

            registry().with(fmt_layer).init();
            }
        });
    }
}
