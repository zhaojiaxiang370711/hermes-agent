//! Command-line interface for boxingAgent (Phase 1a: scaffold + config).
//!
//! Command names mirror hermes_cli/main.py: `_AGENT_COMMANDS = {None, "chat", ...}`
//! (no subcommand ⇒ chat) and `config` is a builtin subcommand.
use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "boxing-agent", version, about = "Faithful Rust port of the Hermes agent core")]
pub struct Cli {
    /// Override the model id (e.g. "example-model").
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Override the provider key.
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Override the system prompt.
    #[arg(long, global = true)]
    pub system: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the agent interactively (Phase 1b).
    Chat {
        /// Initial prompt / task (optional).
        prompt: Vec<String>,
    },
    /// Read/write the shared ~/.hermes/config.yaml.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Model selection (Phase 1b).
    Model,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Get a dotted-path value (e.g. "agent.max_turns").
    Get { key: String },
    /// Set a dotted-path value (e.g. "agent.max_turns 30").
    Set { key: String, value: String },
    /// List keys at a path (top level if omitted).
    List { key: Option<String> },
}

/// Minimal built-in system prompt (overridable via `--system`).
const DEFAULT_SYSTEM: &str = "You are boxingAgent, a helpful assistant.";

/// Resolve the configured provider, build an Agent, stream one turn to stdout.
async fn run_chat(
    model: Option<String>,
    system: Option<String>,
    prompt: Vec<String>,
) -> anyhow::Result<()> {
    let message = prompt.join(" ");
    if message.trim().is_empty() {
        anyhow::bail!("no prompt: provide a message, e.g. `boxing-agent chat \"hello\"`");
    }

    let config_path = boxing_config::config_path()?;
    let env_path = boxing_config::env_path()?;
    let config = boxing_config::load(&config_path)?;

    let provider = boxing_providers::resolve(&config, &env_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let model = match model {
        Some(m) => m,
        None => config
            .get("model.default")
            .map_err(|e| anyhow::anyhow!("model.default: {e}"))?,
    };
    let system = system.unwrap_or_else(|| DEFAULT_SYSTEM.to_string());

    let agent = boxing_core::Agent::new(provider, model, system);
    agent
        .run(&message, &mut |delta| print!("{delta}"))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!();
    Ok(())
}

/// Entry point dispatched from `main`.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Destructure up front so the chat arm can use model/system without a
    // partial-move on cli.command. `..` skips `provider` (the global --provider
    // flag stays a no-op this slice; the loop uses config's model.provider).
    let Cli { model, system, command, .. } = cli;
    match command {
        // Bare `boxing-agent` (no subcommand) enters chat with no prompt,
        // which run_chat rejects with a helpful message. (A bare positional
        // message like `boxing-agent "hi"` isn't supported by this clap tree —
        // use `boxing-agent chat "hi"`.)
        None => run_chat(model, system, Vec::new()).await,
        Some(Command::Chat { prompt }) => run_chat(model, system, prompt).await,
        Some(Command::Model) => {
            eprintln!("boxing-agent: model selection is implemented in a later phase.");
            Ok(())
        }
        Some(Command::Config { action }) => {
            run_config_at(&boxing_config::config_path()?, action)
        }
    }
}

/// Run a `config` subcommand against an explicit path (testable; no env).
pub fn run_config_at(path: &Path, action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::Get { key } => {
            let doc = boxing_config::load(path)?;
            let val = doc.get(&key).map_err(|e| anyhow::anyhow!("{}", e))?;
            println!("{val}");
        }
        ConfigAction::Set { key, value } => {
            let mut doc = boxing_config::load_or_default(path)?;
            doc.set(&key, &value).map_err(|e| anyhow::anyhow!("{}", e))?;
            boxing_config::save(path, &doc)?;
            println!("set {key} = {value}");
        }
        ConfigAction::List { key } => {
            let doc = boxing_config::load(path)?;
            let k = key.as_deref().unwrap_or("");
            for name in doc.list(k).map_err(|e| anyhow::anyhow!("{}", e))? {
                println!("{name}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_is_chat_default() {
        let cli = Cli::try_parse_from(["boxing-agent"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn global_flags_parse() {
        let cli = Cli::try_parse_from(["boxing-agent", "--model", "m", "--provider", "p"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("m"));
        assert_eq!(cli.provider.as_deref(), Some("p"));
    }

    #[test]
    fn config_get_set_list_parse() {
        Cli::try_parse_from(["boxing-agent", "config", "get", "agent.max_turns"]).unwrap();
        Cli::try_parse_from(["boxing-agent", "config", "set", "agent.max_turns", "30"]).unwrap();
        Cli::try_parse_from(["boxing-agent", "config", "list"]).unwrap();
        Cli::try_parse_from(["boxing-agent", "config", "list", "agent"]).unwrap();
    }

    #[test]
    fn unknown_command_rejected() {
        assert!(Cli::try_parse_from(["boxing-agent", "dashboard"]).is_err());
    }

    #[test]
    fn chat_system_and_prompt_flags_parse() {
        let cli = Cli::try_parse_from([
            "boxing-agent", "chat", "--system", "be brief", "do", "the", "thing",
        ])
        .unwrap();
        assert_eq!(cli.system.as_deref(), Some("be brief"));
        let Command::Chat { prompt } = cli.command.expect("chat command") else {
            panic!("expected Chat");
        };
        assert_eq!(prompt, vec!["do", "the", "thing"]);
    }
}
