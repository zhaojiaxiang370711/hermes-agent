//! Faithful config interop for hermes-rs.

pub mod doc;
pub mod path;

pub use doc::ConfigDoc;
pub use path::{config_path, env_path, hermes_home, resolve_hermes_home};
