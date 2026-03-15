/// Configuration for an [`OpfsProject`](crate::OpfsProject) instance.
///
/// All fields have sensible defaults via [`Config::default()`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory for the tgz store (default: `/stores`)
    pub store_root: std::path::PathBuf,
    /// Maximum bytes for the in-memory tar index cache (default: 100 MB)
    pub tar_cache_max_bytes: usize,
    /// Maximum entries in the fuse-link path cache (default: 10 000)
    pub fuse_cache_max_entries: usize,
    /// Maximum concurrent HTTP downloads (default: 20)
    pub max_concurrent_downloads: usize,
    /// Number of retry attempts for failed downloads (default: 3)
    pub download_retries: u32,
    /// Base delay in ms for exponential back-off between retries (default: 500)
    pub retry_base_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store_root: std::path::PathBuf::from("/stores"),
            tar_cache_max_bytes: 100 * 1024 * 1024,
            fuse_cache_max_entries: 10_000,
            max_concurrent_downloads: 20,
            download_retries: 3,
            retry_base_delay_ms: 500,
        }
    }
}
