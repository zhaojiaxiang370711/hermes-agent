//! Faithful config interop for boxingAgent.

pub mod doc;
pub mod envfile;
pub mod path;

pub use doc::{load, load_or_default, save, ConfigDoc};
pub use envfile::env_value;
pub use path::{config_path, env_path, hermes_home, resolve_hermes_home, state_db_path};
