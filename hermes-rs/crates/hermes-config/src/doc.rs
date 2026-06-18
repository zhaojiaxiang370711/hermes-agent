//! Generic YAML config document.
//!
//! Operates on serde_yaml::Value (a Mapping at the root). serde_yaml 0.9
//! Mapping preserves insertion order, so writeback matches the Python
//! original's layout and unknown keys survive untouched.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum GetError {
    #[error("key not found: {0}")]
    NotFound(String),
}

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

    /// Get a value at a dotted path. Scalars render as their literal string;
    /// mappings/sequences render as a YAML block (matches `hermes config get`).
    pub fn get(&self, dotted: &str) -> Result<String, GetError> {
        let node = self.lookup(dotted)?;
        Ok(scalar_or_block(node))
    }

    /// List the string keys under a path (top level if `dotted` is empty).
    pub fn list(&self, dotted: &str) -> Result<Vec<String>, GetError> {
        let node = if dotted.is_empty() { &self.root } else { self.lookup(dotted)? };
        match node {
            serde_yaml::Value::Mapping(m) => Ok(m
                .keys()
                .filter_map(|k| match k {
                    serde_yaml::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()),
            _ => Err(GetError::NotFound(dotted.to_string())),
        }
    }

    fn lookup(&self, dotted: &str) -> Result<&serde_yaml::Value, GetError> {
        let mut cursor = &self.root;
        for (i, part) in dotted.split('.').enumerate() {
            let so_far = dotted.split('.').take(i + 1).collect::<Vec<_>>().join(".");
            let key = serde_yaml::Value::String(part.to_string());
            cursor = match cursor {
                serde_yaml::Value::Mapping(m) => m.get(&key).ok_or(GetError::NotFound(so_far))?,
                _ => return Err(GetError::NotFound(so_far)),
            };
        }
        Ok(cursor)
    }
}

fn scalar_or_block(v: &serde_yaml::Value) -> String {
    use serde_yaml::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
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

    #[test]
    fn get_scalar_nested() {
        let doc = ConfigDoc::from_str(FIXTURE).unwrap();
        assert_eq!(doc.get("agent.max_turns").unwrap(), "60");
        assert_eq!(doc.get("model.provider").unwrap(), "example-provider");
    }

    #[test]
    fn get_missing_key_errors() {
        let doc = ConfigDoc::from_str(FIXTURE).unwrap();
        let err = doc.get("agent.does_not_exist").unwrap_err();
        assert!(matches!(err, GetError::NotFound(_)));
    }

    #[test]
    fn get_mapping_renders_block() {
        let doc = ConfigDoc::from_str(FIXTURE).unwrap();
        let block = doc.get("model").unwrap();
        assert!(block.contains("default: example-model"));
    }

    #[test]
    fn list_top_level_and_nested() {
        let doc = ConfigDoc::from_str(FIXTURE).unwrap();
        assert_eq!(doc.list("").unwrap(), vec!["model", "providers", "fallback_providers", "toolsets", "agent"]);
        assert_eq!(doc.list("agent").unwrap(), vec!["max_turns", "gateway_timeout"]);
    }
}
