pub mod config;
pub mod ipc;
pub mod types;

use std::path::PathBuf;

/// Chemin du socket Unix du daemon : `$XDG_RUNTIME_DIR/clipvault.sock`.
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("clipvault.sock")
}
