//! Generic YAML config document.
//!
//! Operates on serde_yaml::Value (a Mapping at the root). serde_yaml 0.9
//! Mapping preserves insertion order, so writeback matches the Python
//! original's layout and unknown keys survive untouched.

use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct ConfigDoc {
    pub(crate) root: serde_yaml::Value,
}

impl ConfigDoc {
    pub fn from_str(text: &str) -> anyhow::Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let root: serde_yaml::Value = serde_yaml::from_str(text)?;
        Ok(Self { root })
    }

    pub fn to_string(&self) -> String {
        if matches!(self.root, serde_yaml::Value::Null) {
            return String::new();
        }
        serde_yaml::to_string(&self.root).unwrap_or_default()
    }
}

/// Load a config document from disk.
pub fn load(path: &Path) -> anyhow::Result<ConfigDoc> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    ConfigDoc::from_str(&text)
}

/// Save a config document to disk (creates parent dirs).
pub fn save(path: &Path, doc: &ConfigDoc) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string())
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    Ok(())
}

const FIXTURE: &str = "\
model:
  default: example-model
  provider: example-provider
providers:
  example-provider:
    name: Example
    base_url: http://example.test/v1
    key_env: EXAMPLE_API_KEY
    default_model: example-model
fallback_providers: []
toolsets:
- hermes-cli
agent:
  max_turns: 60
  gateway_timeout: 1800
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_top_level_order_and_unknowns() {
        let doc = ConfigDoc::from_str(FIXTURE).unwrap();
        let out = doc.to_string();
        // Top-level key order must be preserved exactly.
        let tops: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with(' ') && l.contains(':'))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        assert_eq!(tops, vec!["model", "providers", "fallback_providers", "toolsets", "agent"]);
    }

    #[test]
    fn round_trip_is_stable() {
        let once = ConfigDoc::from_str(FIXTURE).unwrap().to_string();
        let twice = ConfigDoc::from_str(&once).unwrap().to_string();
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_is_default() {
        let doc = ConfigDoc::from_str("").unwrap();
        assert_eq!(doc.to_string(), "");
    }
}
