//! HERMES_HOME resolution, mirroring hermes_cli/config.py.
//!
//! Resolution order: `$HERMES_HOME` if set, else `$HOME/.hermes`.
//! The pure core `resolve_hermes_home` is split out so tests avoid env races.
use std::path::{Path, PathBuf};

/// Pure resolver — no env access. Testable without process-global state.
pub fn resolve_hermes_home(
    env_home: Option<&Path>,
    home: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = env_home {
        return Ok(p.to_path_buf());
    }
    let h = home.ok_or_else(|| anyhow::anyhow!("HOME is not set; set HERMES_HOME or HOME"))?;
    Ok(h.join(".hermes"))
}

/// Resolve HERMES_HOME from the process environment.
pub fn hermes_home() -> anyhow::Result<PathBuf> {
    let env_home = std::env::var_os("HERMES_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_hermes_home(env_home.as_deref(), home.as_deref())
}

/// `<HERMES_HOME>/config.yaml`
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(hermes_home()?.join("config.yaml"))
}

/// `<HERMES_HOME>/.env`
pub fn env_path() -> anyhow::Result<PathBuf> {
    Ok(hermes_home()?.join(".env"))
}

/// `<HERMES_HOME>/state.db`
pub fn state_db_path() -> anyhow::Result<PathBuf> {
    Ok(hermes_home()?.join("state.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn env_home_wins() {
        let p =
            resolve_hermes_home(Some(Path::new("/tmp/xx")), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/xx"));
    }

    #[test]
    fn falls_back_to_home_dot_hermes() {
        let p = resolve_hermes_home(None, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.hermes"));
    }

    #[test]
    fn missing_home_is_error() {
        let err = resolve_hermes_home(None, None).unwrap_err();
        assert!(err.to_string().contains("HOME is not set"));
    }

    #[test]
    fn config_env_and_state_paths() {
        let home = resolve_hermes_home(Some(Path::new("/tmp/xx")), None).unwrap();
        assert_eq!(
            home.join("config.yaml"),
            PathBuf::from("/tmp/xx/config.yaml")
        );
        assert_eq!(home.join(".env"), PathBuf::from("/tmp/xx/.env"));
        assert_eq!(home.join("state.db"), PathBuf::from("/tmp/xx/state.db"));
    }
}
