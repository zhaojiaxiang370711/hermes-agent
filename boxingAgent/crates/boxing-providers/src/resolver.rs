//! Config / `.env` resolver — builds the right `Provider` from `config.yaml`.
//!
//! Reads the selected provider (`model.provider` → `providers.<key>`), pulls its
//! `base_url` and the API key (named by `key_env`) from `~/.hermes/.env`, and
//! returns a boxed OpenAI-compatible or Anthropic client picked by the
//! provider key / base_url host. Mirrors how the Python original selects a
//! provider and resolves its key.

use std::path::Path;

use boxing_config::ConfigDoc;

use crate::{
    Anthropic, OpenAiCompat, Provider, ProviderError,
};

/// Build the configured provider, resolving its API key from `env_path`.
///
/// `env_path` is normally `~/.hermes/.env` (`boxing_config::env_path()`); the
/// key is looked up by the name in `providers.<key>.key_env`.
pub fn resolve(config: &ConfigDoc, env_path: &Path) -> Result<Box<dyn Provider>, ProviderError> {
    let provider_key = config
        .get("model.provider")
        .map_err(|e| ProviderError::Config(format!("model.provider: {e}")))?;

    let base_url = config
        .get(&format!("providers.{provider_key}.base_url"))
        .map_err(|e| ProviderError::Config(format!("providers.{provider_key}.base_url: {e}")))?;

    let key_env = config
        .get(&format!("providers.{provider_key}.key_env"))
        .map_err(|e| ProviderError::Config(format!("providers.{provider_key}.key_env: {e}")))?;

    let api_key = boxing_config::env_value(env_path, &key_env).ok_or_else(|| {
        ProviderError::Config(format!(
            "API key '{key_env}' (providers.{provider_key}.key_env) not found in {}",
            env_path.display()
        ))
    })?;

    if is_anthropic(&provider_key, &base_url) {
        Ok(Box::new(Anthropic::new(base_url, api_key)))
    } else {
        Ok(Box::new(OpenAiCompat::new(base_url, api_key)))
    }
}

/// Anthropic iff the provider key is `anthropic` or the base_url points at
/// api.anthropic.com; everything else is treated as OpenAI-compatible.
fn is_anthropic(provider_key: &str, base_url: &str) -> bool {
    provider_key == "anthropic" || base_url.contains("api.anthropic.com")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ChatRequest};
    use std::path::PathBuf;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Write a temp `.env`; unique per process so parallel tests don't collide.
    fn temp_env(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("{name}-{}.env", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    fn config_yaml(provider: &str, base_url: &str, key_env: &str) -> String {
        format!(
            "model:\n  default: m\n  provider: {provider}\nproviders:\n  {provider}:\n    name: P\n    base_url: {base_url}\n    key_env: {key_env}\n"
        )
    }

    #[tokio::test]
    async fn resolves_openai_compat_and_dispatches() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "ok"}}]
            })))
            .mount(&server)
            .await;

        let doc = ConfigDoc::from_str(&config_yaml("qxtech", &server.uri(), "QXTECH_API_KEY")).unwrap();
        let env = temp_env("hermes-resolver-openai", "QXTECH_API_KEY=secret");

        let p = resolve(&doc, &env).unwrap();
        let req = ChatRequest::new("gpt-test", vec![ChatMessage::new("user", "hi")]);
        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.content, "ok");
    }

    #[tokio::test]
    async fn resolves_anthropic_and_dispatches() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "antkey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}]
            })))
            .mount(&server)
            .await;

        let doc = ConfigDoc::from_str(&config_yaml("anthropic", &server.uri(), "ANTHROPIC_API_KEY")).unwrap();
        let env = temp_env("hermes-resolver-anthropic", "ANTHROPIC_API_KEY=antkey");

        let p = resolve(&doc, &env).unwrap();
        let mut req = ChatRequest::new("claude", vec![ChatMessage::new("user", "hi")]);
        req.max_tokens = Some(16);
        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.content, "ok");
    }

    #[test]
    fn is_anthropic_detects_by_key_or_host() {
        // Catalog detection rule: anthropic iff provider key is "anthropic" or
        // the base_url host is api.anthropic.com; everything else OpenAI-compat.
        assert!(is_anthropic("anthropic", "https://api.anthropic.com"));
        assert!(is_anthropic("my-claude", "https://api.anthropic.com/v1"));
        assert!(!is_anthropic("qxtech", "http://qxtech.xyz:3001/v1"));
        assert!(!is_anthropic("openai", "https://api.openai.com/v1"));
    }

    #[test]
    fn resolve_errors_when_key_missing_in_env() {
        let doc = ConfigDoc::from_str(&config_yaml("qxtech", "http://localhost:1", "NOPE_KEY")).unwrap();
        let env = temp_env("hermes-resolver-missing", "OTHER=1");
        match resolve(&doc, &env) {
            Err(ProviderError::Config(_)) => {}
            Err(e) => panic!("expected Config error, got {e:?}"),
            Ok(_) => panic!("expected Config error, got Ok"),
        }
    }

    #[test]
    fn resolve_errors_when_provider_unset() {
        let doc = ConfigDoc::from_str("model:\n  default: m\n").unwrap();
        let env = temp_env("hermes-resolver-noprov", "X=1");
        assert!(matches!(resolve(&doc, &env), Err(ProviderError::Config(_))));
    }
}
