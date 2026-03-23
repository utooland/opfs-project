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

    // 2. Fetch all packages (cached or download) with integrity verification
    let store = project.store();
    let fuse = project.fuse_fs();
    let max_concurrent = opts
        .max_concurrent_downloads
        .unwrap_or(project.config().max_concurrent_downloads);

    let results: Vec<_> = stream::iter(groups.into_values().map(|g| {
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

    // 3. Create fuse links for all successful fetches, collect errors
    let mut first_error: Option<OpfsError> = None;
    for result in results {
        match result {
            Ok((name, url, targets)) => {
                let tgz_path = store.tgz_path(&name, &url);
                link_and_warm_cache(fuse, &tgz_path, &targets).await?;
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    if let Some(e) = first_error {
        return Err(e);
    }

    Ok(())
}

/// Create fuse links and warm the cache for a set of target paths.
async fn link_and_warm_cache(
    fuse: &crate::fuse_fs::FuseFs,
    tgz_path: &std::path::Path,
    targets: &[String],
) -> std::result::Result<(), OpfsError> {
    let extracted_dir = fuse
        .extract_tgz_to_dir(tgz_path)
        .await
        .map_err(|e| OpfsError::Other(format!("extract tgz: {e}")))?;
    for target in targets {
        let dst = std::path::PathBuf::from(target);
        fuse.create_fuse_link(&extracted_dir, &dst)
            .await
            .map_err(|e| OpfsError::Other(format!("fuse link for {target}: {e}")))?;
        fuse.warm_link_cache(&dst, &extracted_dir);
    }
    Ok(())
}
