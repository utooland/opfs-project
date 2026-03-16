#![cfg(all(target_family = "wasm", target_os = "unknown"))]

//! opfs-project — OPFS filesystem with fuse-link indirection and
//! lazy package installation for WASM applications.
//!
//! # Quick start
//!
//! ```ignore
//! use opfs_project::{OpfsProject, Config};
//!
//! let project = OpfsProject::new(Config::default());
//! let bytes = project.read("node_modules/foo/index.js").await?;
//! ```

// ── modules ──────────────────────────────────────────────────────────────

pub mod archive;
pub mod config;
pub mod error;
pub mod fuse_fs;
pub mod package_lock;
pub mod package_manager;
pub mod project;
pub mod store;

// ── re-exports ───────────────────────────────────────────────────────────

pub use config::Config;
pub use error::{OpfsError, VerifyResult};
pub use package_manager::{InstallOptions, OmitType};
pub use project::OpfsProject;

// ── test utilities ───────────────────────────────────────────────────────

#[cfg(test)]
pub mod test_utils {
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Initialise tracing-web for browser-based tests.
    pub fn init_tracing() {
        INIT.call_once(|| {
            use tracing_subscriber::{
                fmt::{self, format::FmtSpan},
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
        });
    }
}
