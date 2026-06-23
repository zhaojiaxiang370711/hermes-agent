//! Minimal ~/.hermes/.env reader: look up a key in a KEY=value file.
//!
//! Mirrors just enough of shell .env semantics for resolving provider key_env
//! values: ignores blank lines and `#` comments, strips an optional `export `
//! prefix, and strips matching surrounding quotes. No variable expansion.
use std::path::Path;

/// Read the value for `key` from a KEY=value file. None if the file or key is absent.
pub fn env_value(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        return Some(strip_quotes(v.trim()));
    }
    None
}

fn strip_quotes(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"') || s.starts_with('\'') && s.ends_with('\''))
        && s.len() >= 2
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_env(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn reads_plain_key_ignoring_comments() {
        let dir = std::env::temp_dir().join("hermes-env-test-plain");
        let p = write_env(&dir, ".env", "QXTECH_API_KEY=abc123\n# comment\nOTHER=x\n");
        assert_eq!(env_value(&p, "QXTECH_API_KEY").as_deref(), Some("abc123"));
        assert_eq!(env_value(&p, "OTHER").as_deref(), Some("x"));
        assert_eq!(env_value(&p, "MISSING"), None);
    }

    #[test]
    fn handles_export_prefix_and_quotes() {
        let dir = std::env::temp_dir().join("hermes-env-test-export");
        let p = write_env(
            &dir,
            ".env",
            "export FOO=\"bar baz\"\n  QUOTED='single'\nBLANK=\n",
        );
        assert_eq!(env_value(&p, "FOO").as_deref(), Some("bar baz"));
        assert_eq!(env_value(&p, "QUOTED").as_deref(), Some("single"));
        assert_eq!(env_value(&p, "BLANK").as_deref(), Some(""));
    }

    #[test]
    fn missing_file_is_none() {
        assert_eq!(env_value(Path::new("/no/such/hermes/.env"), "X"), None);
    }
}
