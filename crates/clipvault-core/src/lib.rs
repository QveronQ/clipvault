pub mod config;
pub mod ipc;
pub mod sync;
pub mod types;

use std::path::PathBuf;

/// Chemin du socket Unix du daemon : `$CLIPVAULT_SOCKET` s'il est défini
/// (tests, instances multiples), sinon `$XDG_RUNTIME_DIR/clipvault.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CLIPVAULT_SOCKET") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("clipvault.sock")
}
