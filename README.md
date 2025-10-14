# OPFS Project

A Rust library for working with the Origin Private File System (OPFS) in WebAssembly applications.

## Features

- File and directory operations in OPFS
- Support for fuse.link files to create symbolic links between directories
- Package management functionality for handling npm-style dependencies
- Asynchronous API for all file operations

## Use Cases

- Web applications that need persistent local storage
- Package managers for web-based development environments
- Applications that need to manage complex file structures in the browser
- Tools that require fast access to local files without user interaction

## Development Setup

1. Install Rust and Cargo
2. Install wasm-pack: `cargo install wasm-pack`
3. Build the project: `wasm-pack build`
4. Run tests: `wasm-pack test --chrome --headless`

## API

### File Operations

- `opfs::read_dir(path)` - Read directory contents
- `opfs::read(path)` - Read file contents
- `opfs::write(path, content)` - Write content to file
- `opfs::create_dir_all(path)` - Create directory and all parent directories
- `opfs::remove(path)` - Remove a file
- `opfs::exists(path)` - Check if file or directory exists

### Package Management

- `package_manager::install_deps(package_lock)` - Install dependencies from package-lock.json

### Fuse Link Operations

- `fuse::fuse_link(src, dst)` - Create fuse link between source and destination directories
- `fuse::read(path)` - Read file content with fuse.link support
- `fuse::read_dir(path)` - Read directory contents with fuse.link support

## Testing

Tests are written using wasm-bindgen-test and can be run with:

```bash
wasm-pack test --chrome --headless
```

Note: Tests require a modern browser with OPFS support (Firefox 116+, Chrome 114+).

## License

MIT
