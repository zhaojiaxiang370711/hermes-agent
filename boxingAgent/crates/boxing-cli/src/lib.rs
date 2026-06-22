//! Command-line interface for boxingAgent (Phase 1a: scaffold + config).
//!
//! Command names mirror hermes_cli/main.py: `_AGENT_COMMANDS = {None, "chat", ...}`
//! (no subcommand ⇒ chat) and `config` is a builtin subcommand.
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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

    /// Max tokens for provider responses (default: 4096).
    #[arg(long, global = true, default_value = "4096")]
    pub max_tokens: u32,

    /// Max turns for the tool loop (default: 30).
    #[arg(long, global = true, default_value = "30")]
    pub max_turns: usize,

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

/// 构建完整工具集：默认工具 + MCP 工具 + delegate_task（子代理委托，支持异步）。
fn agent_tools(
    provider: Arc<dyn boxing_providers::Provider>,
    model: &str,
    system: &str,
    max_turns: usize,
    max_tokens: u32,
    config: &boxing_config::ConfigDoc,
) -> Vec<Box<dyn boxing_tools::Tool>> {
    let mut tools = boxing_tools::default_tools();

    // 发现并注册 MCP 工具
    if let Ok(mcp_yaml) = config.get("mcp_servers") {
        if !mcp_yaml.is_empty() {
            match serde_yaml::from_str::<HashMap<String, boxing_tools::mcp::McpServerConfig>>(&mcp_yaml) {
                Ok(servers) if !servers.is_empty() => {
                    eprintln!("MCP: 发现 {} 个配置的服务器", servers.len());
                    let mcp_tools = boxing_tools::mcp::discover_mcp_tools(&servers);
                    eprintln!("MCP: 共发现 {} 个工具", mcp_tools.len());
                    tools.extend(mcp_tools);
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("MCP: 解析 mcp_servers 配置失败: {e}");
                }
            }
        }
    }

    // 创建异步委托注册表
    let async_registry = Arc::new(boxing_core::AsyncDelegationRegistry::new());

    // 同步委托（background=false 时使用）
    tools.push(Box::new(
        boxing_core::Delegate::new(
            provider.clone(),
            model.to_string(),
            system.to_string(),
            max_turns,
            max_tokens,
            0, // depth
        ).with_async_registry(Arc::clone(&async_registry))
    ));

    // 异步委托（background=true 时使用）
    tools.push(Box::new(boxing_core::AsyncDelegate::new(
        provider,
        model.to_string(),
        system.to_string(),
        max_turns,
        max_tokens,
        0, // depth
        async_registry,
    )));

    tools
}

/// Resolve the configured provider, build an Agent, stream one turn to stdout.
async fn run_chat(
    model: Option<String>,
    system: Option<String>,
    prompt: Vec<String>,
    max_tokens: u32,
    max_turns: usize,
) -> anyhow::Result<()> {
    let message = prompt.join(" ");
    if message.trim().is_empty() {
        anyhow::bail!("no prompt: provide a message, e.g. `boxing-agent chat \"hello\"`");
    }

    let config_path = boxing_config::config_path()?;
    let env_path = boxing_config::env_path()?;
    let config = boxing_config::load(&config_path)?;

    let provider = Arc::from(
        boxing_providers::resolve(&config, &env_path)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );

    let model = match model {
        Some(m) => m,
        None => config
            .get("model.default")
            .map_err(|e| anyhow::anyhow!("model.default: {e}"))?,
    };
    let system = system.unwrap_or_else(|| DEFAULT_SYSTEM.to_string());

    let tools = agent_tools(Arc::clone(&provider), &model, &system, max_turns, max_tokens, &config);
    let mut agent = boxing_core::Agent::new(provider, model, system, tools, max_turns, max_tokens);

    // 启用记忆自动注入
    let hermes_home = match boxing_config::hermes_home() {
        Ok(h) => h,
        Err(_) => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            std::path::PathBuf::from(home).join(".hermes")
        }
    };
    let memory_injector = boxing_core::MemoryInjector::load(&hermes_home);
    agent = agent.with_memory(memory_injector);

    match boxing_state::SessionStore::open(&boxing_config::state_db_path()?) {
        Ok(store) => {
            agent = agent.with_store(store);
        }
        Err(e) => eprintln!("boxing-agent: state.db 不可用，以 ephemeral 模式运行: {e}"),
    }
    let _answer = agent
        .run(
            &message,
            &mut |delta| print!("{delta}"),
            &mut |ev| match ev {
                boxing_core::LoopEvent::ToolCall { name } => eprintln!("→ {name}"),
                boxing_core::LoopEvent::ToolResult { name, ok } => {
                    eprintln!("{} {name}", if ok { "✓" } else { "✗" })
                }
                boxing_core::LoopEvent::MaxTurns => eprintln!("boxing-agent: 达到最大轮数"),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!();
    Ok(())
}

/// Entry point dispatched from `main`.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let Cli { model, system, max_tokens, max_turns, command, .. } = cli;
    match command {
        None => run_chat(model, system, Vec::new(), max_tokens, max_turns).await,
        Some(Command::Chat { prompt }) => run_chat(model, system, prompt, max_tokens, max_turns).await,
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
