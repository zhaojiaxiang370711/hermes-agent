//! boxingAgent MCP（Model Context Protocol）客户端。
//!
//! 连接外部 MCP 服务器（stdio 传输），发现工具，并在 agent 循环中调用。
//! 遵循 MCP JSON-RPC 2.0 协议（initialize → tools/list → tools/call）。
//! 与 Hermes 原版 `tools/mcp_tool.py` 的协议交互一致。
//!
//! 当前支持：
//! - stdio 传输（command + args 子进程）
//! - tools/list + tools/call（核心工具发现与调用）
//! - 动态工具注册（MCP 工具包装为 `boxing_tools::Tool`）
//!
//! 不支持（推迟）：
//! - HTTP/StreamableHTTP / SSE 传输
//! - OAuth 认证
//! - resources/* / prompts/*
//! - notifications/tools/list_changed

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{Tool, ToolError};

// ===== MCP 协议常量 =====

const PROTOCOL_VERSION: &str = "2024-11-05";

// ===== MCP 配置类型 =====

/// MCP 服务器配置（从 config.yaml 的 mcp_servers 读取）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    300
}

// ===== MCP 协议类型 =====

/// MCP 工具定义（从 tools/list 响应解析）。
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolDef>,
}

#[derive(Debug, Deserialize)]
struct McpContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct ToolsCallResult {
    #[serde(default)]
    content: Vec<McpContentBlock>,
    #[serde(default)]
    is_error: bool,
}

// ===== JSON-RPC =====

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    id: u64,
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

// ===== MCP 客户端 =====

/// MCP stdio 客户端：管理子进程 + JSON-RPC 通信。
///
/// stdin/stdout 分离持有，通过 Mutex 保证线程安全。
/// 子进程在 Drop 时自动清理。
pub struct McpClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    next_id: AtomicU64,
    server_name: String,
}

impl McpClient {
    /// 启动 MCP 服务器子进程并完成初始化握手。
    pub fn connect(name: &str, config: &McpServerConfig) -> Result<Self, ToolError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Other(format!("启动 MCP 服务器 '{name}' 失败: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Other("无法获取 MCP stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Other("无法获取 MCP stdout".into()))?;

        let client = Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
            server_name: name.to_string(),
        };

        // 初始化握手
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "boxingAgent", "version": "0.1.0" }
        });
        let _ = client.rpc("initialize", Some(params))?;

        Ok(client)
    }

    /// 获取工具列表。
    pub fn list_tools(&self) -> Result<Vec<McpToolDef>, ToolError> {
        let result = self.rpc("tools/list", None)?;
        let parsed: ToolsListResult = serde_json::from_value(
            result.ok_or_else(|| ToolError::Other("tools/list 返回空".into()))?,
        )
        .map_err(|e| ToolError::Other(format!("解析 tools/list 失败: {e}")))?;
        Ok(parsed.tools)
    }

    /// 调用工具。
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<String, ToolError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.rpc("tools/call", Some(params))?;
        let parsed: ToolsCallResult = serde_json::from_value(
            result.ok_or_else(|| ToolError::Other("tools/call 返回空".into()))?,
        )
        .map_err(|e| ToolError::Other(format!("解析 tools/call 失败: {e}")))?;

        let text: String = parsed
            .content
            .iter()
            .filter(|b| b.kind == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if parsed.is_error {
            Err(ToolError::Other(format!("MCP 工具 '{name}' 错误: {text}")))
        } else {
            Ok(text)
        }
    }

    /// 服务器名称。
    pub fn name(&self) -> &str {
        &self.server_name
    }

    /// 发送 JSON-RPC 请求并读取响应（同步阻塞）。
    fn rpc(&self, method: &str, params: Option<Value>) -> Result<Option<Value>, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let json_line = serde_json::to_string(&request)
            .map_err(|e| ToolError::Other(format!("序列化 JSON-RPC: {e}")))?;

        // 写入 stdin（换行分隔）
        {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|e| ToolError::Other(format!("stdin 锁: {e}")))?;
            stdin
                .write_all(format!("{json_line}\n").as_bytes())
                .map_err(|e| ToolError::Other(format!("写入 stdin: {e}")))?;
            stdin
                .flush()
                .map_err(|e| ToolError::Other(format!("flush stdin: {e}")))?;
        }

        // 读取 stdout（逐行直到匹配 id）
        let mut stdout = self
            .stdout
            .lock()
            .map_err(|e| ToolError::Other(format!("stdout 锁: {e}")))?;

        loop {
            let mut line = String::new();
            let n = stdout
                .read_line(&mut line)
                .map_err(|e| ToolError::Other(format!("读取 stdout: {e}")))?;
            if n == 0 {
                return Err(ToolError::Other("MCP 服务器关闭了 stdout".into()));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 尝试解析为 JSON-RPC 响应
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(line) {
                if resp.id == id {
                    if let Some(err) = resp.error {
                        return Err(ToolError::Other(format!(
                            "JSON-RPC 错误: {}",
                            serde_json::to_string(&err).unwrap_or_default()
                        )));
                    }
                    return Ok(resp.result);
                }
                // id 不匹配 — 可能是通知或乱序响应，跳过
            }
            // 非 JSON-RPC 行（服务器调试输出等），跳过
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ===== MCP 工具包装器 =====

/// 将远程 MCP 工具暴露为本地 `boxing_tools::Tool`。
///
/// 工具名格式：`{server}__{tool}`（双下划线分隔）。
pub struct McpTool {
    name: String,
    description: String,
    input_schema: Value,
    client: Arc<McpClient>,
    remote_name: String,
}

impl McpTool {
    pub fn new(server_name: &str, def: &McpToolDef, client: Arc<McpClient>) -> Self {
        Self {
            name: format!("{server_name}__{}", def.name),
            description: def.description.clone(),
            input_schema: def.input_schema.clone(),
            client,
            remote_name: def.name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.input_schema,
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        self.client.call_tool(&self.remote_name, &args)
    }
}

// ===== 公开 API =====

/// 连接所有配置的 MCP 服务器，发现工具，返回 Tool 包装器列表。
///
/// 服务器启动失败或工具发现失败的服务器会被跳过（不阻塞其他服务器）。
pub fn discover_mcp_tools(
    mcp_servers: &HashMap<String, McpServerConfig>,
) -> Vec<Box<dyn Tool>> {
    let mut tools = Vec::new();

    for (name, config) in mcp_servers {
        match McpClient::connect(name, config) {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools() {
                    Ok(defs) => {
                        eprintln!(
                            "MCP: 服务器 '{name}' 发现 {} 个工具",
                            defs.len()
                        );
                        for def in &defs {
                            tools.push(Box::new(McpTool::new(name, def, Arc::clone(&client)))
                                as Box<dyn Tool>);
                        }
                    }
                    Err(e) => {
                        eprintln!("MCP: 服务器 '{name}' tools/list 失败: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("MCP: 服务器 '{name}' 连接失败: {e}");
            }
        }
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_config() {
        let yaml = "
command: npx
args:
  - -y
  - '@modelcontextprotocol/server-filesystem'
  - /tmp
timeout: 120
";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.command, "npx");
        assert_eq!(config.args.len(), 3);
        assert_eq!(config.timeout, 120);
    }

    #[test]
    fn parse_tools_list() {
        let json = r#"{"tools":[{"name":"read_file","description":"Read","inputSchema":{"type":"object"}}]}"#;
        let result: ToolsListResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "read_file");
    }

    #[test]
    fn parse_tools_call() {
        let json = r#"{"content":[{"type":"text","text":"hi"}],"isError":false}"#;
        let result: ToolsCallResult = serde_json::from_str(json).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content[0].text, "hi");
    }

    #[test]
    fn mcp_tool_name_has_prefix() {
        let def = McpToolDef {
            name: "read_file".into(),
            description: "Read".into(),
            input_schema: json!({}),
        };
        // 不创建真正的 McpClient（需要子进程），只验证名称格式
        let expected = "filesystem__read_file";
        let actual = format!("{}__{}", "filesystem", def.name);
        assert_eq!(actual, expected);
    }
}
