//! Types shared by the hedgehog CLI and daemon.

pub mod api;
pub mod env;
pub mod manifest;
pub mod names;
pub mod paths;

pub use manifest::Manifest;
pub use paths::Paths;

/// The port the daemon's control API listens on.
pub const API_PORT: u16 = 7700;
