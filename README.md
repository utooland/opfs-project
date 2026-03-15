# OPFS Project

A Rust library for managing npm-style projects on the Origin Private File System (OPFS) in WebAssembly. Provides lazy tgz extraction via fuse links, a content-addressable store, and zero-copy file reads.

## Features

- **Struct-based API** — all state owned by `OpfsProject` (config, caches, store)
- **Fuse links** — `fuse.link` files map `node_modules/<pkg>/` to tgz entries in the store, enabling lazy on-demand extraction
- **Zero-copy reads** — decompressed tar kept as a single `Bytes` buffer; individual files served via `Bytes::slice()` (no per-file allocations)
- **O(1) lookups** — `is_dir`, `get_file`, `list_dir` all backed by `HashMap`/`HashSet`
- **LRU cache** — configurable budget for decompressed tar data with automatic eviction
- **In-flight dedup** — concurrent reads of the same tgz only decompress once
- **Content-addressable store** — tgz files stored by integrity hash (SHA-512 / SHA-1)
- **Async I/O** — all file operations are async via `tokio-fs-ext`

## Architecture

```
OpfsProject
├── Config         — store root, cache budgets, download settings
├── FuseFs         — fuse-link resolution + tar index
│   ├── link_cache — PathBuf → Arc<FuseLink> (LRU)
│   └── tar_index  — PathBuf → TgzEntry (LRU, zero-copy Bytes)
└── Store          — download, verify (sha512/sha1), save tgz
```

## Usage

```rust
use opfs_project::{OpfsProject, Config};

// Create with defaults
let project = OpfsProject::default();

// Or customise
let project = OpfsProject::new(Config {
    store_root: "/my-store".into(),
    tar_cache_max_bytes: 200 * 1024 * 1024, // 200MB
    ..Config::default()
});

// Fuse-aware reads (transparent tgz extraction under node_modules)
let content = project.read("node_modules/react/index.js").await?;
let entries = project.read_dir("node_modules/react/lib").await?;

// Install from package-lock.json
use opfs_project::package_lock::PackageLock;
let lock = PackageLock::from_json(json_str)?;
project.install(&lock, &Default::default()).await?;
```

## Testing

```bash
# native tests
cargo test

# wasm tests (requires chromedriver)
brew install --cask chromedriver  # macOS
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown

# interactive wasm tests
brew install wasm-pack
wasm-pack test --chrome
```

## License

MIT
