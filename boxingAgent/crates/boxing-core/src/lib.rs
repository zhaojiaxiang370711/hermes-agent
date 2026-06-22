//! boxingAgent agent loop.
//!
//! Minimal Phase-2d slice: a single turn that assembles [system, user],
//! streams the provider response, renders each delta, and returns the full
//! text. `run()` is structured as one `step()` today so future tool-use can
//! branch inside the step without rewriting the loop.

use boxing_providers::{ChatMessage, ChatRequest, Provider, ProviderError};
use futures::StreamExt;

pub struct Agent {
    provider: Box<dyn Provider>,
    model: String,
    system: String,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>, model: String, system: String) -> Self {
        Self { provider, model, system }
    }

    /// One user turn: assemble [system?, user], stream the response, render each
    /// delta via `on_delta`, and return the full concatenated text. Today this
    /// is exactly one provider call (one `step()`); when tool-use lands, the
    /// tool branch lives inside `step()` and `run()` iterates steps until a
    /// no-tool stop.
    pub async fn run(
        &self,
        user_message: &str,
        on_delta: &mut impl FnMut(&str),
    ) -> Result<String, ProviderError> {
        let mut messages = Vec::with_capacity(2);
        if !self.system.is_empty() {
            messages.push(ChatMessage::new("system", self.system.as_str()));
        }
        messages.push(ChatMessage::new("user", user_message));
        let text = self.step(messages, on_delta).await?;
        Ok(text)
    }

    /// The single provider-call unit: stream a ChatRequest, render + accumulate.
    async fn step(
        &self,
        messages: Vec<ChatMessage>,
        on_delta: &mut impl FnMut(&str),
    ) -> Result<String, ProviderError> {
        let mut req = ChatRequest::new(self.model.as_str(), messages);
        req.stream = true;
        let mut stream = self.provider.stream(&req).await?;
        let mut out = String::new();
        while let Some(delta) = stream.next().await {
            let delta = delta?;
            on_delta(&delta.content);
            out.push_str(&delta.content);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Fake provider yielding canned deltas; records the ChatRequest it saw.
    /// One helper backs all four tests (with-system and empty-system cases).
    fn agent_with_capture(
        system: &str,
        deltas: &[&str],
    ) -> (Agent, Arc<Mutex<Option<ChatRequest>>>) {
        struct CapturingStub {
            deltas: Vec<String>,
            last: Arc<Mutex<Option<ChatRequest>>>,
        }
        #[async_trait::async_trait]
        impl Provider for CapturingStub {
            async fn complete(
                &self,
                _req: &ChatRequest,
            ) -> Result<boxing_providers::ChatResponse, ProviderError> {
                unreachable!("run() uses stream(), not complete()")
            }
            async fn stream(&self, req: &ChatRequest) -> Result<boxing_providers::ChatStream, ProviderError> {
                *self.last.lock().unwrap() = Some(req.clone());
                let d: Vec<Result<boxing_providers::TokenDelta, ProviderError>> = self
                    .deltas
                    .iter()
                    .map(|s| Ok(boxing_providers::TokenDelta::new((*s).to_string())))
                    .collect();
                Ok(Box::pin(futures::stream::iter(d)))
            }
        }
        let last = Arc::new(Mutex::new(None));
        let stub = CapturingStub {
            deltas: deltas.iter().map(|s| s.to_string()).collect(),
            last: Arc::clone(&last),
        };
        (
            Agent::new(Box::new(stub), "model-x".to_string(), system.to_string()),
            last,
        )
    }

    #[tokio::test]
    async fn run_assembles_system_then_user_request() {
        let (agent, last) = agent_with_capture("SYS", &["hi"]);
        let _ = agent.run("hello", &mut |_| {}).await.unwrap();
        let req = last.lock().unwrap().clone().unwrap();
        assert_eq!(req.model, "model-x");
        assert!(req.stream);
        assert_eq!(
            req.messages,
            vec![
                ChatMessage::new("system", "SYS"),
                ChatMessage::new("user", "hello"),
            ]
        );
    }

    #[tokio::test]
    async fn run_returns_concatenated_deltas() {
        let (agent, _) = agent_with_capture("SYS", &["Hel", "lo", " world"]);
        let text = agent.run("hi", &mut |_| {}).await.unwrap();
        assert_eq!(text, "Hello world");
    }

    #[tokio::test]
    async fn run_invokes_on_delta_per_token_in_order() {
        let (agent, _) = agent_with_capture("SYS", &["a", "b", "c"]);
        let mut seen = Vec::new();
        agent.run("hi", &mut |d| seen.push(d.to_string())).await.unwrap();
        assert_eq!(seen, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn run_omits_system_message_when_empty() {
        let (agent, last) = agent_with_capture("", &["ok"]);
        let _ = agent.run("hi", &mut |_| {}).await.unwrap();
        let req = last.lock().unwrap().clone().unwrap();
        assert_eq!(req.messages, vec![ChatMessage::new("user", "hi")]);
    }
}
