use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use crate::package_lock::PackageLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Maximum number of entries in package metadata cache
/// This should be large enough to hold all packages from package-lock.json
/// For large projects, this might be 50000+ entries, but each entry is small (~200 bytes)
const MAX_METADATA_CACHE_SIZE: usize = 100000;

/// Maximum number of entries in fetched directories cache
/// This cache is just for optimization to avoid redundant HTTP requests
/// Can be safely limited as it will fallback to re-fetch from HTTP or OPFS cache
const MAX_FETCHED_DIRS_CACHE_SIZE: usize = 5000;

/// Global cache for package metadata from package-lock.json
/// Key: install_path (e.g., "node_modules/debug"), Value: package metadata
static PACKAGE_METADATA_CACHE: LazyLock<RwLock<HashMap<String, PackageMetadata>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Cache to track which directories have already been fetched and created
/// Key: absolute path (e.g., "/utooweb-demo/node_modules/antd/lib")
static FETCHED_DIRS_CACHE: LazyLock<RwLock<std::collections::HashSet<PathBuf>>> =
    LazyLock::new(|| RwLock::new(std::collections::HashSet::new()));

/// Package metadata extracted from package-lock.json
#[derive(Debug, Clone)]
struct PackageMetadata {
    name: String,
    version: String,
    registry_url: String, // Base registry URL (e.g., "https://registry.npmmirror.com")
    install_path: String, // e.g., "node_modules/debug"
}

/// File entry from registry API
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub file_type: String, // "file" or "directory"
    pub size: Option<u64>,
    pub modified: Option<String>,
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub file_count: usize,
    pub total_size: u64,
}

/// Response from /?meta endpoint
#[derive(Debug, Deserialize)]
struct FileListResponse {
    pub files: Vec<FileEntry>,
}

/// Extract host from URL
/// Example: "https://registry.npmmirror.com/debug/..." -> "registry.npmmirror.com"
fn extract_host(url: &str) -> String {
    url.split('/').nth(2).unwrap_or("unknown").to_string()
}

/// Construct cache file path
/// Example: registry.npmmirror.com/antd/5.0.0/lib/index.js
///       -> /registry-fs/registry.npmmirror.com/antd/5.0.0/lib/index.js
fn get_cache_file_path(metadata: &PackageMetadata, relative_path: &str) -> PathBuf {
    let registry_host = extract_host(&metadata.registry_url);
    let cache_base = PathBuf::from("/registry-fs");
    cache_base
        .join(registry_host)
        .join(&metadata.name)
        .join(&metadata.version)
        .join(relative_path)
}

/// Construct directory list metadata cache path
/// Example: /registry-fs/registry.npmmirror.com/antd/5.0.0/.meta-lib.json
fn get_cache_meta_path(metadata: &PackageMetadata, subpath: &str) -> PathBuf {
    let registry_host = extract_host(&metadata.registry_url);
    let cache_base = PathBuf::from("/registry-fs");
    let meta_filename = if subpath.is_empty() {
        ".meta-root.json".to_string()
    } else {
        format!(".meta-{}.json", subpath.replace('/', "-"))
    };
    cache_base
        .join(registry_host)
        .join(&metadata.name)
        .join(&metadata.version)
        .join(meta_filename)
}

impl PackageMetadata {
    /// Parse registry URL and extract base URL
    /// Example: "https://registry.antgroup-inc.cn/debug/-/debug-2.6.9.tgz"
    ///       -> "https://registry.antgroup-inc.cn"
    fn parse_registry_url(resolved: &str) -> String {
        if let Some(url) = resolved.split('/').take(3).collect::<Vec<_>>().get(0..3) {
            url.join("/")
        } else {
            // Fallback to npmmirror
            "https://registry.npmmirror.com".to_string()
        }
    }

    /// Construct file list URL
    /// Example: https://registry.npmmirror.com/debug/4.4.3/files/?meta
    ///          https://registry.npmmirror.com/debug/4.4.3/files/src/?meta
    fn file_list_url(&self, subpath: &str) -> String {
        let path_segment = if subpath.is_empty() {
            "/".to_string()
        } else {
            format!("/{}/", subpath.trim_start_matches('/').trim_end_matches('/'))
        };

        format!(
            "{}/{}/{}/files{}?meta",
            self.registry_url, self.name, self.version, path_segment
        )
    }

    /// Construct file content URL
    /// Example: https://registry.npmmirror.com/debug/4.4.3/files/src/node.js
    fn file_content_url(&self, file_path: &str) -> String {
        let path = file_path.trim_start_matches('/');
        format!(
            "{}/{}/{}/files/{}",
            self.registry_url, self.name, self.version, path
        )
    }
}

/// Check and enforce metadata cache size limit
/// Note: This cache contains package-lock.json mapping, removing entries may cause packages not found.
/// Only remove entries when cache is significantly over limit.
fn check_metadata_cache_size(cache: &mut HashMap<String, PackageMetadata>) {
    // Only clean up if significantly over limit (2x)
    if cache.len() > MAX_METADATA_CACHE_SIZE * 2 {
        let to_remove = cache.len() - MAX_METADATA_CACHE_SIZE;
        warn!("Metadata cache size ({}) significantly exceeded limit ({}), removing {} entries",
              cache.len(), MAX_METADATA_CACHE_SIZE, to_remove);

        // Remove random entries to get back to the limit
        let keys_to_remove: Vec<String> = cache.keys()
            .take(to_remove)
            .cloned()
            .collect();

        for key in keys_to_remove {
            cache.remove(&key);
        }

        info!("Metadata cache size after cleanup: {}", cache.len());
    }
}

/// Check and enforce fetched dirs cache size limit
fn check_fetched_dirs_cache_size(cache: &mut std::collections::HashSet<PathBuf>) {
    if cache.len() > MAX_FETCHED_DIRS_CACHE_SIZE {
        let to_remove = cache.len() - MAX_FETCHED_DIRS_CACHE_SIZE / 2;
        warn!("Fetched dirs cache size ({}) exceeded limit ({}), removing {} oldest entries",
              cache.len(), MAX_FETCHED_DIRS_CACHE_SIZE, to_remove);

        // Remove random entries to get back to half capacity
        let paths_to_remove: Vec<PathBuf> = cache.iter()
            .take(to_remove)
            .cloned()
            .collect();

        for path in paths_to_remove {
            cache.remove(&path);
        }

        info!("Fetched dirs cache size after cleanup: {}", cache.len());
    }
}

/// Initialize registry filesystem by loading package-lock.json
pub async fn init_from_package_lock<P: AsRef<Path>>(lock_file_path: P) -> Result<()> {
    info!("Initializing registry filesystem from package-lock.json");

    // Read package-lock.json
    let lock_content = tokio_fs_ext::read_to_string(lock_file_path.as_ref()).await?;
    let package_lock = PackageLock::from_json(&lock_content)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Failed to parse package-lock.json: {}", e)))?;

    info!("Parsed package-lock.json with {} packages", package_lock.packages.len());

    // Extract package metadata
    let mut metadata_cache = PACKAGE_METADATA_CACHE.write()
        .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to acquire lock: {}", e)))?;

    for (install_path, lock_package) in package_lock.packages.iter() {
        // Skip root package (empty path)
        if install_path.is_empty() {
            continue;
        }

        // Skip packages without resolved URL
        let resolved = match &lock_package.resolved {
            Some(r) => r,
            None => {
                info!("Skipping package {} without resolved URL", install_path);
                continue;
            }
        };

        let name = lock_package.get_name(install_path);
        let version = lock_package.get_version();
        let registry_url = PackageMetadata::parse_registry_url(resolved);

        let metadata = PackageMetadata {
            name: name.clone(),
            version: version.clone(),
            registry_url,
            install_path: install_path.clone(),
        };

        metadata_cache.insert(install_path.clone(), metadata);
    }

    info!("Successfully initialized {} packages", metadata_cache.len());

    // Check cache size after initialization
    check_metadata_cache_size(&mut metadata_cache);

    Ok(())
}

/// Get package metadata for a given path
/// First tries memory cache, then falls back to reading package-lock.json from disk
async fn get_package_metadata(path: &Path) -> Result<Option<PackageMetadata>> {
    let path_str = path.to_string_lossy();

    // Convert absolute path to relative path (relative to cwd)
    let cwd = crate::get_cwd();
    let cwd_str = cwd.to_string_lossy();

    let relative_path_str = if let Some(rel) = path_str.strip_prefix(&*cwd_str) {
        rel.trim_start_matches('/')
    } else {
        &path_str
    };

    info!("Looking up metadata: path={}, relative={}", path_str, relative_path_str);

    // Try memory cache first
    let metadata_cache = PACKAGE_METADATA_CACHE.read()
        .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to acquire lock: {}", e)))?;

    let mut best_match: Option<&PackageMetadata> = None;
    let mut best_match_len = 0;

    for (install_path, metadata) in metadata_cache.iter() {
        if relative_path_str.starts_with(install_path) {
            info!("  Match found in cache: {} starts with {}", relative_path_str, install_path);
            if install_path.len() > best_match_len {
                best_match = Some(metadata);
                best_match_len = install_path.len();
            }
        }
    }

    if let Some(metadata) = best_match {
        return Ok(Some(metadata.clone()));
    }

    // Cache miss, try to load from disk
    drop(metadata_cache); // Release read lock before doing disk I/O

    info!("  Memory cache miss. Falling back to disk lookup from package-lock.json");

    // Try to find matching package from package-lock.json on disk
    let lock_file_path = cwd.join("package-lock.json");
    let lock_content = match tokio_fs_ext::read_to_string(&lock_file_path).await {
        Ok(content) => content,
        Err(e) => {
            info!("  Failed to read package-lock.json from disk: {:?}", e);
            return Ok(None);
        }
    };

    let package_lock = match PackageLock::from_json(&lock_content) {
        Ok(lock) => lock,
        Err(e) => {
            warn!("  Failed to parse package-lock.json: {:?}", e);
            return Ok(None);
        }
    };

    // Find the longest matching install path from package-lock.json
    let mut best_metadata: Option<PackageMetadata> = None;
    let mut best_len = 0;

    for (install_path, lock_package) in package_lock.packages.iter() {
        if install_path.is_empty() || !relative_path_str.starts_with(install_path) {
            continue;
        }

        if install_path.len() > best_len {
            // Found a better match
            if let Some(resolved) = &lock_package.resolved {
                let name = lock_package.get_name(install_path);
                let version = lock_package.get_version();
                let registry_url = PackageMetadata::parse_registry_url(resolved);

                best_metadata = Some(PackageMetadata {
                    name,
                    version,
                    registry_url,
                    install_path: install_path.clone(),
                });
                best_len = install_path.len();
            }
        }
    }

    if let Some(ref metadata) = best_metadata {
        info!("  Found metadata from disk: {} @ {}", metadata.name, metadata.version);

        // Try to add back to cache (best effort, ignore if cache is full)
        if let Ok(mut cache) = PACKAGE_METADATA_CACHE.write() {
            if cache.len() < MAX_METADATA_CACHE_SIZE * 2 {
                cache.insert(metadata.install_path.clone(), metadata.clone());
                info!("  Added metadata back to cache");
            } else {
                info!("  Cache is full, skipping cache update");
            }
        }
    } else {
        info!("  No matching package found in package-lock.json");
    }

    Ok(best_metadata)
}

/// Get relative path within package
/// Example: "/utooweb-demo/node_modules/debug/src/node.js" with install_path "node_modules/debug"
///       -> "src/node.js"
fn get_relative_path_in_package(path: &Path, install_path: &str) -> String {
    let path_str = path.to_string_lossy();

    // Convert absolute path to relative path (relative to cwd)
    let cwd = crate::get_cwd();
    let cwd_str = cwd.to_string_lossy();

    let relative_path_str = if let Some(rel) = path_str.strip_prefix(&*cwd_str) {
        rel.trim_start_matches('/')
    } else {
        &path_str
    };

    // Now strip the install_path prefix
    if let Some(relative) = relative_path_str.strip_prefix(install_path) {
        relative.trim_start_matches('/').to_string()
    } else {
        String::new()
    }
}

/// Check if path is the root node_modules directory (${cwd}/node_modules)
/// Returns true only for first-level node_modules, not nested ones
fn is_root_node_modules(path: &Path) -> bool {
    let cwd = crate::get_cwd();
    let root_node_modules = cwd.join("node_modules");
    path == root_node_modules
}

/// Check if path is a scope directory (e.g., node_modules/@ant-design)
/// Returns Some(scope_name) if it is, None otherwise
fn is_scope_directory(path: &Path) -> Option<String> {
    let cwd = crate::get_cwd();
    let path_str = path.to_string_lossy();
    let cwd_str = cwd.to_string_lossy();

    let relative_path_str = if let Some(rel) = path_str.strip_prefix(&*cwd_str) {
        rel.trim_start_matches('/')
    } else {
        &path_str
    };

    // Check if it matches "node_modules/@scope" pattern
    if let Some(after_nm) = relative_path_str.strip_prefix("node_modules/") {
        if after_nm.starts_with('@') && !after_nm.contains('/') {
            return Some(after_nm.to_string());
        }
    }

    None
}

/// Check if path is the current working directory
fn is_cwd(path: &Path) -> bool {
    let cwd = crate::get_cwd();
    let path_str = path.to_string_lossy();
    let cwd_str = cwd.to_string_lossy();

    // Check various forms of current directory
    let is_match = path == cwd
        || path == cwd.join(".")
        || path_str == format!("{}.", cwd_str.trim_end_matches('/'))
        || path_str == format!("{}/.", cwd_str.trim_end_matches('/'));

    is_match
}

/// Ensure package metadata is initialized
async fn ensure_initialized() -> Result<()> {
    let is_empty = PACKAGE_METADATA_CACHE.read()
        .map(|c| c.is_empty())
        .unwrap_or(true);

    if is_empty {
        info!("Auto-initializing from package-lock.json");
        let cwd = crate::get_cwd();
        let lock_file_path = cwd.join("package-lock.json");
        init_from_package_lock(&lock_file_path).await?;
    }

    Ok(())
}

/// Handle reading scope directory (e.g., node_modules/@ant-design)
/// Extracts all packages under this scope from package-lock.json
async fn handle_scope_directory(path: &Path, scope_name: &str) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    info!("Handling scope directory: {} (scope: {})", path.display(), scope_name);

    // Ensure initialized
    ensure_initialized().await?;

    // Get all packages under this scope from cache
    let metadata_cache = PACKAGE_METADATA_CACHE.read()
        .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to acquire lock: {}", e)))?;

    let mut packages_in_scope = std::collections::HashSet::new();
    let scope_prefix = format!("node_modules/{}/", scope_name);

    for install_path in metadata_cache.keys() {
        if let Some(after_scope) = install_path.strip_prefix(&scope_prefix) {
            // Only get direct children, not nested
            if !after_scope.contains('/') && !after_scope.contains("node_modules") {
                packages_in_scope.insert(after_scope.to_string());
            }
        }
    }

    drop(metadata_cache);

    info!("Found {} packages in scope {} from package-lock.json", packages_in_scope.len(), scope_name);

    // Read physical scope directory if it exists (overlay mode)
    let mut physical_packages = std::collections::HashSet::new();
    if let Ok(physical_entries) = crate::util::read_dir_direct(path).await {
        for entry in physical_entries {
            let name = entry.file_name().to_string_lossy().to_string();
            physical_packages.insert(name);
        }
        info!("Found {} physical packages in scope {}", physical_packages.len(), scope_name);
    }

    // Merge: virtual packages overlay physical ones
    let mut all_packages = packages_in_scope;
    all_packages.extend(physical_packages);

    // Create virtual DirEntry objects for packages in this scope
    let mut dir_entries = Vec::new();
    for package_name in all_packages {
        let package_path = path.join(&package_name);
        let dir_entry = tokio_fs_ext::DirEntry::new(
            package_path,
            OsString::from(&package_name),
            tokio_fs_ext::FileType::Directory,
        );
        dir_entries.push(dir_entry);
    }

    info!("Created {} total entries for scope {} (overlay mode)", dir_entries.len(), scope_name);

    // Log all entries
    for entry in &dir_entries {
        info!("  - {}", entry.file_name().to_string_lossy());
    }

    Ok(Some(dir_entries))
}

/// Handle reading root node_modules directory from package-lock.json
/// Extracts first-level package names and creates shadow directory structure
async fn handle_root_node_modules(path: &Path) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    info!("Handling root node_modules: read package-lock.json");

    // Ensure initialized
    ensure_initialized().await?;

    // Read package-lock.json
    let cwd = crate::get_cwd();
    let lock_file_path = cwd.join("package-lock.json");

    let lock_content = tokio_fs_ext::read_to_string(&lock_file_path).await
        .map_err(|e| {
            warn!("Failed to read package-lock.json: {:?}", e);
            e
        })?;

    let package_lock = PackageLock::from_json(&lock_content)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Failed to parse package-lock.json: {}", e)))?;

    // Extract first-level package names
    let mut first_level_packages = std::collections::HashSet::new();

    for install_path in package_lock.packages.keys() {
        if install_path.is_empty() || !install_path.starts_with("node_modules/") {
            continue;
        }

        let after_nm = &install_path["node_modules/".len()..];
        if after_nm.contains("node_modules") {
            continue; // Skip nested
        }

        // Extract package name (handle scoped packages)
        let package_name = if after_nm.starts_with('@') {
            after_nm.split('/').take(2).collect::<Vec<_>>().join("/")
        } else {
            after_nm.split('/').next().unwrap().to_string()
        };

        first_level_packages.insert(package_name);
    }

    info!("Found {} first-level packages from package-lock.json", first_level_packages.len());

    // Read physical node_modules if it exists (overlay mode)
    let mut physical_packages = std::collections::HashSet::new();
    if let Ok(physical_entries) = crate::util::read_dir_direct(path).await {
        for entry in physical_entries {
            let name = entry.file_name().to_string_lossy().to_string();
            physical_packages.insert(name);
        }
        info!("Found {} physical packages in node_modules", physical_packages.len());
    }

    // Merge: virtual packages overlay physical ones
    let mut all_packages = first_level_packages;
    all_packages.extend(physical_packages);

    // Create virtual DirEntry objects for all packages
    let mut dir_entries = Vec::new();
    for package_name in all_packages {
        let package_path = path.join(&package_name);
        let dir_entry = tokio_fs_ext::DirEntry::new(
            package_path,
            OsString::from(&package_name),
            tokio_fs_ext::FileType::Directory,
        );
        dir_entries.push(dir_entry);
    }

    info!("Created {} total entries for root node_modules (overlay mode)", dir_entries.len());

    // Log all entries
    for entry in &dir_entries {
        info!("  - {}", entry.file_name().to_string_lossy());
    }

    Ok(Some(dir_entries))
}

/// Fetch file list from registry with caching
async fn fetch_file_list(metadata: &PackageMetadata, subpath: &str) -> Result<Vec<FileEntry>> {
    let cache_path = get_cache_meta_path(metadata, subpath);

    // 1. Check cache first
    if let Ok(cached_data) = tokio_fs_ext::read(&cache_path).await {
        if let Ok(cached_entries) = serde_json::from_slice::<Vec<FileEntry>>(&cached_data) {
            info!("Loaded file list from cache: {:?} ({} entries)", cache_path, cached_entries.len());
            return Ok(cached_entries);
        }
    }

    // 2. Cache miss, fetch from HTTP
    let url = metadata.file_list_url(subpath);
    info!("Cache miss, fetching file list from: {}", url);

    let response = reqwest::get(&url)
        .await
        .map_err(|e| Error::new(ErrorKind::Other, format!("HTTP request failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("Failed to fetch file list: HTTP {}", response.status())
        ));
    }

    let file_list_response: FileListResponse = response
        .json()
        .await
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Failed to parse response: {}", e)))?;

    let entries = file_list_response.files;
    info!("Fetched {} entries for {} @ {}", entries.len(), metadata.name, metadata.version);

    // 3. Write to cache
    if let Ok(cache_json) = serde_json::to_vec(&entries) {
        if let Some(parent) = cache_path.parent() {
            tokio_fs_ext::create_dir_all(parent).await.ok();
        }
        if let Err(e) = tokio_fs_ext::write(&cache_path, &cache_json).await {
            warn!("Failed to save file list cache: {:?}", e);
        } else {
            info!("Saved file list to cache: {:?}", cache_path);
        }
    }

    Ok(entries)
}


/// Try to read file through registry filesystem
pub(crate) async fn try_read_through_registry<P: AsRef<Path> + std::fmt::Debug>(
    path: P,
) -> Result<Option<Vec<u8>>> {
    let path_ref = path.as_ref();

    info!("Attempting to read file through registry FS: {:?}", path);

    // Auto-initialize if needed
    ensure_initialized().await?;

    // Get package metadata
    let metadata = match get_package_metadata(path_ref).await? {
        Some(m) => m,
        None => {
            info!("No package metadata found for path: {:?}", path);
            return Ok(None);
        }
    };

    // Get relative path within package
    let relative_path = get_relative_path_in_package(path_ref, &metadata.install_path);

    if relative_path.is_empty() {
        // Trying to read the package directory itself
        info!("Cannot read directory as file: {:?}", path);
        return Ok(None);
    }

    let cache_path = get_cache_file_path(&metadata, &relative_path);

    // 1. Check cache first
    if let Ok(cached_content) = tokio_fs_ext::read(&cache_path).await {
        if !cached_content.is_empty() {
            info!("Loaded file from cache: {:?} ({} bytes)", cache_path, cached_content.len());
            return Ok(Some(cached_content));
        }
    }

    // 2. Cache miss, fetch from HTTP
    let url = metadata.file_content_url(&relative_path);
    info!("Cache miss, fetching file from: {}", url);

    let response = reqwest::get(&url)
        .await
        .map_err(|e| Error::new(ErrorKind::Other, format!("HTTP request failed: {}", e)))?;

    if !response.status().is_success() {
        warn!("Failed to fetch file: HTTP {}", response.status());
        return Ok(None);
    }

    let content = response
        .bytes()
        .await
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Failed to read response: {}", e)))?
        .to_vec();

    info!("Fetched {} bytes for {:?}", content.len(), path);

    // 3. Write to cache
    if let Some(parent) = cache_path.parent() {
        tokio_fs_ext::create_dir_all(parent).await.ok();
    }
    if let Err(e) = tokio_fs_ext::write(&cache_path, &content).await {
        warn!("Failed to save file cache: {:?}", e);
    } else {
        info!("Saved file to cache: {:?}", cache_path);
    }

    Ok(Some(content))
}

/// Try to read directory through registry filesystem
pub(crate) async fn try_read_dir_through_registry<P: AsRef<Path> + std::fmt::Debug>(
    path: P,
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    let path_ref = path.as_ref();

    info!("=== Registry read_dir called with: {:?} ===", path);

    // Handle current working directory - inject virtual node_modules
    if is_cwd(path_ref) {
        info!("    -> is current working directory, handling with overlay...");

        // Always add virtual node_modules (overlay behavior)
        let mut entries = match crate::util::read_dir_direct(path_ref).await {
            Ok(entries) => {
                // Filter out physical node_modules if it exists
                // We'll replace it with our virtual one (overlay)
                entries.into_iter()
                    .filter(|e| e.file_name().to_string_lossy() != "node_modules")
                    .collect()
            }
            Err(_) => Vec::new(),
        };

        // Add virtual node_modules (this overlays any physical one)
        let cwd = crate::get_cwd();
        let node_modules_path = cwd.join("node_modules");
        let virtual_nm = tokio_fs_ext::DirEntry::new(
            node_modules_path,
            OsString::from("node_modules"),
            tokio_fs_ext::FileType::Directory,
        );
        entries.push(virtual_nm);
        info!("    Added virtual node_modules directory (overlay mode)");

        // Log all entries
        info!("    Returning {} entries from cwd:", entries.len());
        for entry in &entries {
            info!("      - {}", entry.file_name().to_string_lossy());
        }

        return Ok(Some(entries));
    }

    // Handle root node_modules
    if is_root_node_modules(path_ref) {
        return handle_root_node_modules(path_ref).await;
    }

    // Handle scope directory (e.g., node_modules/@ant-design)
    if let Some(scope_name) = is_scope_directory(path_ref) {
        return handle_scope_directory(path_ref, &scope_name).await;
    }

    // Ensure initialized
    ensure_initialized().await?;

    // Get package metadata
    let metadata = match get_package_metadata(path_ref).await? {
        Some(m) => m,
        None => {
            info!("No package metadata for: {:?}", path);
            return Ok(None);
        }
    };

    // Get relative path within package
    let relative_path = get_relative_path_in_package(path_ref, &metadata.install_path);
    info!("Metadata install_path: '{}'", metadata.install_path);

    // Check if we've already fetched this directory
    let path_buf = path_ref.to_path_buf();
    {
        let fetched_cache = FETCHED_DIRS_CACHE.read()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to acquire lock: {}", e)))?;

        if fetched_cache.contains(&path_buf) {
            info!("Directory already fetched, reading from disk: {:?}", path_ref);
            // Read and return the existing directory from disk
            return crate::util::read_dir_direct(path_ref).await.map(Some);
        }
    }

    // Fetch file list from HTTP API
    let entries = fetch_file_list(&metadata, &relative_path).await?;

    // Create virtual DirEntry objects without creating actual files
    let mut dir_entries = Vec::new();

    for entry in &entries {
        // Strip leading slash from entry.path
        let entry_path_clean = entry.path.trim_start_matches('/');

        // If relative_path is not empty, entry.path contains it, so strip it
        let entry_relative = if !relative_path.is_empty() {
            entry_path_clean
                .strip_prefix(&format!("{}/", relative_path))
                .unwrap_or(entry_path_clean)
        } else {
            entry_path_clean
        };

        // Extract just the name (last component of the path)
        let name = entry_relative.split('/').last().unwrap_or(entry_relative);

        // Skip if this is a nested path (contains /)
        if entry_relative.contains('/') {
            continue;
        }

        let entry_path = path_ref.join(name);
        let file_type = tokio_fs_ext::FileType::from_str(&entry.file_type);

        info!("  - Virtual entry: {} (type: {})", name, entry.file_type);

        let dir_entry = tokio_fs_ext::DirEntry::new(
            entry_path,
            OsString::from(name),
            file_type,
        );

        dir_entries.push(dir_entry);
    }

    // Mark directory as fetched
    {
        let mut fetched_cache = FETCHED_DIRS_CACHE.write()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to acquire lock: {}", e)))?;
        fetched_cache.insert(path_buf);
        info!("Marked directory as fetched: {:?}", path_ref);

        // Check and enforce cache size limit
        check_fetched_dirs_cache_size(&mut fetched_cache);
    }

    info!("Created {} virtual entries for {:?}", dir_entries.len(), path_ref);
    Ok(Some(dir_entries))
}

/// Clear all caches
pub fn clear_cache() {
    if let Ok(mut cache) = PACKAGE_METADATA_CACHE.write() {
        cache.clear();
    }
    if let Ok(mut cache) = FETCHED_DIRS_CACHE.write() {
        cache.clear();
    }
    info!("Cleared registry filesystem caches");
}

/// Get package metadata cache statistics for debugging
/// Returns metadata_count
pub fn get_cache_stats() -> usize {
    PACKAGE_METADATA_CACHE.read().map(|c| c.len()).unwrap_or(0)
}

/// Clear all registry filesystem caches (both metadata and file caches)
pub async fn clear_all_registry_cache() -> Result<()> {
    info!("Clearing all registry caches...");

    // Clear file cache directory
    if let Err(e) = tokio_fs_ext::remove_dir_all("/registry-fs").await {
        warn!("Failed to clear registry-fs cache directory: {:?}", e);
    } else {
        info!("Cleared /registry-fs cache directory");
    }

    Ok(())
}
