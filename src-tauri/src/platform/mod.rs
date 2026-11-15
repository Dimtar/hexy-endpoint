//! Per-OS mounting. Every backend exposes the same three functions and never
//! puts the password on a command line, where other local users could read it.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::*;

/// A share of the server that is currently mounted on this machine.
#[derive(Debug, Clone)]
pub struct Mounted {
    pub share: String,
    /// Where it lives locally: a path, a drive letter, or a gvfs URI.
    pub location: String,
}
