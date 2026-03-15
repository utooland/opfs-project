//! Package install orchestration.
//!
//! This module handles only the install workflow — grouping packages by
//! tgz URL, downloading via [`Store`], and creating fuse links via
//! [`FuseFs`]. All I/O is delegated to those subsystems.

use std::collections::HashMap;

use futures::stream::{self, StreamExt};

use crate::error::OpfsError;
use crate::package_lock::{LockPackage, PackageLock};
use crate::project::OpfsProject;

/// Types of dependencies that can be omitted during install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmitType {
    /// Skip dev dependencies (packages with `dev: true`)
    Dev,
    /// Skip optional dependencies (packages with `optional: true`)
    Optional,
}

/// Options for [`OpfsProject::install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Maximum concurrent downloads (overrides [`Config::max_concurrent_downloads`])
    pub max_concurrent_downloads: Option<usize>,
    /// Types of dependencies to skip
    pub omit: Vec<OmitType>,
}

// ── internal grouping ────────────────────────────────────────────────────

struct PackageGroup {
    name: String,
    version: String,
    tgz_url: String,
    integrity: Option<String>,
    shasum: Option<String>,
    target_paths: Vec<String>,
}

fn should_omit(pkg: &LockPackage, omit: &[OmitType]) -> bool {
    omit.iter().any(|o| match o {
        OmitType::Dev => pkg.dev == Some(true),
        OmitType::Optional => pkg.optional == Some(true),
    })
}

// ── public entry point ───────────────────────────────────────────────────

/// Install all packages from a lock file.
///
/// Called by [`OpfsProject::install`] — not intended for direct use.
pub(crate) async fn install(
    project: &OpfsProject,
    lock: &PackageLock,
    opts: &InstallOptions,
) -> Result<(), OpfsError> {
    let omit = &opts.omit;

    // 1. Group packages by tgz URL (deduplication)
    let mut groups: HashMap<String, PackageGroup> = HashMap::new();

    for (path, pkg) in lock.packages.iter().filter(|(p, _)| !p.is_empty()) {
        if should_omit(pkg, omit) {
            tracing::debug!("{}@{}: skipped", pkg.get_name(path), pkg.get_version());
            continue;
        }

        // Skip optional packages with platform constraints (binary, won't work in WASM)
        if pkg.optional == Some(true) && (pkg.os.is_some() || pkg.cpu.is_some()) {
            tracing::debug!(
                "{}@{}: skipped (platform-specific optional)",
                pkg.get_name(path),
                pkg.get_version()
            );
            continue;
        }

        let name = pkg.get_name(path);
        let version = pkg.get_version();
        let tgz_url = match &pkg.resolved {
            Some(u) => u.clone(),
            None => {
                tracing::warn!("{name}@{version}: no resolved URL, skipping");
                continue;
            }
        };

        groups
            .entry(tgz_url.clone())
            .or_insert_with(|| PackageGroup {
                name,
                version,
                tgz_url,
                integrity: pkg.integrity.clone(),
                shasum: pkg.shasum.clone(),
                target_paths: Vec::new(),
            })
            .target_paths
            .push(path.clone());
    }

    // 2. Partition: cached vs needs download
    let store = project.store();
    let fuse = project.fuse_fs();
    let mut cached = Vec::new();
    let mut to_download = Vec::new();

    for group in groups.into_values() {
        if store.is_cached(&group.name, &group.tgz_url).await {
            let tgz_path = store.tgz_path(&group.name, &group.tgz_url);
            cached.push((tgz_path, group.target_paths));
        } else {
            to_download.push(group);
        }
    }

    // 3. Create fuse links for cached packages + warm caches
    let lazy_tgz = project.config().lazy_tgz;
    for (tgz_path, targets) in &cached {
        link_and_warm_cache(fuse, lazy_tgz, tgz_path, targets).await?;
    }

    // 4. Download and link the rest
    if !to_download.is_empty() {
        let max_concurrent = opts
            .max_concurrent_downloads
            .unwrap_or(project.config().max_concurrent_downloads);

        let results: Vec<_> = stream::iter(to_download.into_iter().map(|g| {
            let store = project.store();
            async move {
                store
                    .fetch_tgz(
                        &g.name,
                        &g.version,
                        &g.tgz_url,
                        g.integrity.as_deref(),
                        g.shasum.as_deref(),
                    )
                    .await?;
                Ok::<_, OpfsError>((g.name, g.tgz_url, g.target_paths))
            }
        }))
        .buffer_unordered(max_concurrent)
        .collect()
        .await;

        for result in results {
            let (name, url, targets) = result?;
            let tgz_path = store.tgz_path(&name, &url);
            link_and_warm_cache(fuse, lazy_tgz, &tgz_path, &targets).await?;
        }
    }

    Ok(())
}

/// Create fuse links and warm the cache for a set of target paths.
async fn link_and_warm_cache(
    fuse: &crate::fuse_fs::FuseFs,
    lazy_tgz: bool,
    tgz_path: &std::path::Path,
    targets: &[String],
) -> std::result::Result<(), OpfsError> {
    let (link_target, prefix) = if lazy_tgz {
        (tgz_path.to_path_buf(), Some("package"))
    } else {
        let out = fuse
            .extract_tgz_to_dir(tgz_path)
            .await
            .map_err(|e| OpfsError::Other(format!("extract tgz: {e}")))?;
        (out, None)
    };
    for target in targets {
        let dst = std::path::PathBuf::from(target);
        fuse.create_fuse_link(&link_target, &dst, prefix)
            .await
            .map_err(|e| OpfsError::Other(format!("fuse link for {target}: {e}")))?;
        fuse.warm_link_cache(&dst, &link_target, prefix);
    }
    Ok(())
}
