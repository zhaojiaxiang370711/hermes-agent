//! boxingAgent MCP（Model Context Protocol）客户端。
//!
//! 连接外部 MCP 服务器，发现工具，并在 agent 循环中调用。
//! 遵循 MCP JSON-RPC 2.0 协议。
//!
//! 传输：stdio / HTTP / SSE
//! 安全：环境变量过滤 + 凭证脱敏
//! 协议：tools/list + tools/call + notifications/initialized

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::oauth::{OAuthClient, OAuthConfig};
use crate::{Tool, ToolError};

const PROTOCOL_VERSION: &str = "2024-11-05";

// ===== 安全 =====

const SAFE_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "TMPDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PUBLIC",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
    "COMSPEC",
    "OS",
    "PATHEXT",
];

fn build_safe_env(user_env: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in std::env::vars() {
        if SAFE_ENV_KEYS.contains(&key.as_str()) || key.starts_with("XDG_") {
            env.insert(key, value);
        }
    }
    if let Some(extra) = user_env {
        for (k, v) in extra {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

fn sanitize_error(text: &str) -> String {
    let mut result = text.to_string();
    for prefix in &[
        "ghp_",
        "sk-",
        "Bearer ",
        "token=",
        "key=",
        "password=",
        "secret=",
    ] {
        while let Some(start) = result.find(prefix) {
            let rest = &result[start..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            result = format!("{}[REDACTED]{}", &result[..start], &result[start + end..]);
        }
    }
    result
}

// ===== 配置 =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub transport: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// OAuth 2.1 配置（需要授权的 HTTP/SSE 服务器）
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
}

fn default_timeout() -> u64 {
    300
}

impl McpServerConfig {
    pub fn transport_type(&self) -> TransportType {
        if !self.url.is_empty() && self.transport == "sse" {
            TransportType::Sse
        } else if !self.url.is_empty() {
            TransportType::Http
        } else {
            TransportType::Stdio
        }
    }
    pub fn is_stdio(&self) -> bool {
        self.transport_type() == TransportType::Stdio
    }
    pub fn is_http(&self) -> bool {
        self.transport_type() == TransportType::Http
    }
    pub fn is_sse(&self) -> bool {
        self.transport_type() == TransportType::Sse
    }
}

#[derive(Debug, PartialEq)]
pub enum TransportType {
    Stdio,
    Http,
    Sse,
}

// ===== 协议类型 =====

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct ListResult {
    #[serde(default)]
    tools: Vec<McpToolDef>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct CallResult {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Serialize)]
struct RpcReq {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct RpcResp {
    id: u64,
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(rename = "method", default)]
    notification_method: Option<String>,
}

// ===== 客户端 =====

/// MCP 客户端：管理传输 + JSON-RPC 通信。
pub struct McpClient {
    transport: TransportType,
    // stdio
    child: Option<Mutex<Child>>,
    stdin: Option<Mutex<ChildStdin>>,
    stdout: Option<Mutex<BufReader<ChildStdout>>>,
    // http/sse
    url: String,
    headers: HashMap<String, String>,
    http_client: Option<reqwest::blocking::Client>,
    // OAuth
    oauth_client: Option<OAuthClient>,
    // 通用
    next_id: AtomicU64,
    server_name: String,
    cached_tools: Mutex<Option<Vec<McpToolDef>>>,
}

impl McpClient {
    /// 连接 MCP 服务器。
    pub fn connect(name: &str, config: &McpServerConfig) -> Result<Self, ToolError> {
        let tt = config.transport_type();
        let timeout = Duration::from_secs(config.timeout);

        let client = match tt {
            TransportType::Stdio => {
                let mut cmd = Command::new(&config.command);
                cmd.args(&config.args);
                for (k, v) in &build_safe_env(Some(&config.env)) {
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

                Self {
                    transport: tt,
                    child: Some(Mutex::new(child)),
                    stdin: Some(Mutex::new(stdin)),
                    stdout: Some(Mutex::new(BufReader::new(stdout))),
                    url: String::new(),
                    headers: HashMap::new(),
                    http_client: None,
                    oauth_client: None,
                    next_id: AtomicU64::new(1),
                    server_name: name.to_string(),
                    cached_tools: Mutex::new(None),
                }
            }
            TransportType::Http | TransportType::Sse => {
                let http_client = reqwest::blocking::Client::builder()
                    .timeout(timeout)
                    .build()
                    .map_err(|e| ToolError::Other(format!("创建 HTTP 客户端: {e}")))?;

                // OAuth 客户端（如果配置了）
                let oauth_client = config
                    .oauth
                    .as_ref()
                    .map(|oc| OAuthClient::new(&config.url, oc, &crate::hermes_home(), name));

                Self {
                    transport: tt,
                    child: None,
                    stdin: None,
                    stdout: None,
                    url: config.url.clone(),
                    headers: config.headers.clone(),
                    http_client: Some(http_client),
                    oauth_client,
                    next_id: AtomicU64::new(1),
                    server_name: name.to_string(),
                    cached_tools: Mutex::new(None),
                }
            }
        };

        // 初始化握手
        let init_params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "boxingAgent", "version": "0.1.0" }
        });
        let _ = client.rpc("initialize", Some(init_params))?;
        // 发送 initialized 通知
        let _ = client.notify("notifications/initialized");

        Ok(client)
    }

    /// 获取工具列表（带缓存）。
    pub fn list_tools(&self) -> Result<Vec<McpToolDef>, ToolError> {
        {
            let cached = self
                .cached_tools
                .lock()
                .map_err(|e| ToolError::Other(e.to_string()))?;
            if let Some(tools) = cached.as_ref() {
                return Ok(tools.clone());
            }
        }
        let result = self.rpc("tools/list", None)?;
        let parsed: ListResult = serde_json::from_value(
            result.ok_or_else(|| ToolError::Other("tools/list 返回空".into()))?,
        )
        .map_err(|e| ToolError::Other(format!("解析 tools/list: {e}")))?;

        let mut cached = self
            .cached_tools
            .lock()
            .map_err(|e| ToolError::Other(e.to_string()))?;
        *cached = Some(parsed.tools.clone());
        Ok(parsed.tools)
    }

    /// 调用工具。
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<String, ToolError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.rpc("tools/call", Some(params))?;
        let parsed: CallResult = serde_json::from_value(
            result.ok_or_else(|| ToolError::Other("tools/call 返回空".into()))?,
        )
        .map_err(|e| ToolError::Other(format!("解析 tools/call: {e}")))?;

        let text: String = parsed
            .content
            .iter()
            .filter(|b| b.kind == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if parsed.is_error {
            Err(ToolError::Other(sanitize_error(&format!(
                "MCP 工具 '{name}' 错误: {text}"
            ))))
        } else {
            Ok(text)
        }
    }

    /// 刷新工具缓存。
    pub fn refresh_tools(&self) -> Result<(), ToolError> {
        let mut cached = self
            .cached_tools
            .lock()
            .map_err(|e| ToolError::Other(e.to_string()))?;
        *cached = None;
        drop(cached);
        self.list_tools()?;
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.server_name
    }

    // ===== 内部 =====

    fn rpc(&self, method: &str, params: Option<Value>) -> Result<Option<Value>, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = RpcReq {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        match self.transport {
            TransportType::Http | TransportType::Sse => {
                let body = serde_json::to_string(&request)
                    .map_err(|e| ToolError::Other(format!("序列化: {e}")))?;
                let client = self.http_client.as_ref().unwrap();
                let mut req = client.post(&self.url).body(body);
                // OAuth: 注入 Authorization Bearer token
                if let Some(oauth) = &self.oauth_client {
                    match oauth.get_access_token() {
                        Ok(token) => {
                            req = req.header("Authorization", format!("Bearer {token}"));
                        }
                        Err(e) => {
                            return Err(ToolError::Other(format!(
                                "MCP OAuth: 无法获取 access_token: {e}"
                            )));
                        }
                    }
                }
                for (k, v) in &self.headers {
                    req = req.header(k, v);
                }
                let resp = req.send().map_err(|e| {
                    ToolError::Other(sanitize_error(&format!("HTTP 请求失败: {e}")))
                })?;
                let status = resp.status();
                let text = resp
                    .text()
                    .map_err(|e| ToolError::Other(format!("读取: {e}")))?;
                if !status.is_success() {
                    return Err(ToolError::Other(sanitize_error(&format!(
                        "HTTP {status}: {text}"
                    ))));
                }
                let parsed: RpcResp = serde_json::from_str(&text)
                    .map_err(|e| ToolError::Other(format!("解析响应: {e}")))?;
                self.check_resp(parsed, id)
            }
            TransportType::Stdio => {
                let json = serde_json::to_string(&request)
                    .map_err(|e| ToolError::Other(format!("序列化: {e}")))?;
                self.send_stdio(&json)?;

                loop {
                    let line = self.recv_stdio()?;
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(resp) = serde_json::from_str::<RpcResp>(&line) {
                        if resp.notification_method.is_some() {
                            // notifications/tools/list_changed → 后台刷新缓存
                            if resp.notification_method.as_deref()
                                == Some("notifications/tools/list_changed")
                            {
                                let _ = self.refresh_tools();
                            }
                            continue;
                        }
                        return self.check_resp(resp, id);
                    }
                    // 调试行，跳过
                }
            }
        }
    }

    fn notify(&self, method: &str) -> Result<(), ToolError> {
        let notification = json!({ "jsonrpc": "2.0", "method": method });
        let json = serde_json::to_string(&notification)
            .map_err(|e| ToolError::Other(format!("序列化通知: {e}")))?;

        match self.transport {
            TransportType::Stdio => {
                self.send_stdio(&json)?;
            }
            TransportType::Http | TransportType::Sse => {
                if let Some(client) = &self.http_client {
                    let mut req = client.post(&self.url).body(json);
                    for (k, v) in &self.headers {
                        req = req.header(k, v);
                    }
                    let _ = req.send();
                }
            }
        }
        Ok(())
    }

    fn send_stdio(&self, line: &str) -> Result<(), ToolError> {
        let stdin = self.stdin.as_ref().unwrap();
        let mut stdin = stdin.lock().map_err(|e| ToolError::Other(e.to_string()))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| ToolError::Other(format!("写入 stdin: {e}")))?;
        stdin
            .flush()
            .map_err(|e| ToolError::Other(format!("flush: {e}")))?;
        Ok(())
    }

    fn recv_stdio(&self) -> Result<String, ToolError> {
        let stdout = self.stdout.as_ref().unwrap();
        let mut stdout = stdout.lock().map_err(|e| ToolError::Other(e.to_string()))?;
        let mut line = String::new();
        let n = stdout
            .read_line(&mut line)
            .map_err(|e| ToolError::Other(format!("读取 stdout: {e}")))?;
        if n == 0 {
            return Err(ToolError::Other("MCP 服务器关闭了 stdout".into()));
        }
        Ok(line.trim().to_string())
    }

    fn check_resp(&self, resp: RpcResp, expected_id: u64) -> Result<Option<Value>, ToolError> {
        if resp.id != expected_id {
            return Err(ToolError::Other(format!(
                "JSON-RPC id 不匹配: 期望 {expected_id}, 收到 {}",
                resp.id
            )));
        }
        if let Some(err) = resp.error {
            return Err(ToolError::Other(sanitize_error(&format!(
                "JSON-RPC 错误: {}",
                serde_json::to_string(&err).unwrap_or_default()
            ))));
        }
        Ok(resp.result)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(child) = &self.child {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

// ===== MCP 工具包装 =====

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
        json!({ "name": self.name, "description": self.description, "parameters": self.input_schema })
    }
    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        self.client.call_tool(&self.remote_name, &args)
    }
}

// ===== 公开 API =====

pub fn discover_mcp_tools(mcp_servers: &HashMap<String, McpServerConfig>) -> Vec<Box<dyn Tool>> {
    let mut tools = Vec::new();
    for (name, config) in mcp_servers {
        match McpClient::connect(name, config) {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools() {
                    Ok(defs) => {
                        eprintln!("MCP: 服务器 '{name}' 发现 {} 个工具", defs.len());
                        for def in &defs {
                            tools.push(Box::new(McpTool::new(name, def, Arc::clone(&client)))
                                as Box<dyn Tool>);
                        }
                    }
                    Err(e) => eprintln!("MCP: 服务器 '{name}' tools/list 失败: {e}"),
                }
            }
            Err(e) => eprintln!(
                "MCP: 服务器 '{name}' 连接失败: {}",
                sanitize_error(&e.to_string())
            ),
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stdio_config() {
        let yaml = "command: npx\nargs:\n  - -y\n  - '@mcp/server'\ntimeout: 120\n";
        let c: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(c.transport_type(), TransportType::Stdio);
        assert_eq!(c.command, "npx");
        assert_eq!(c.args.len(), 2);
    }

    #[test]
    fn parse_http_config() {
        let yaml =
            "url: 'https://mcp.example.com/api'\nheaders:\n  Authorization: 'Bearer sk-test'\n";
        let c: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(c.transport_type(), TransportType::Http);
        assert!(c.url.contains("mcp.example.com"));
    }

    #[test]
    fn parse_sse_config() {
        let yaml = "url: 'http://localhost:8000/sse'\ntransport: sse\n";
        let c: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(c.transport_type(), TransportType::Sse);
    }

    #[test]
    fn parse_list() {
        let j = r#"{"tools":[{"name":"read","description":"R","inputSchema":{"type":"object"}}]}"#;
        let r: ListResult = serde_json::from_str(j).unwrap();
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "read");
    }

    #[test]
    fn parse_call() {
        let j = r#"{"content":[{"type":"text","text":"hi"}],"isError":false}"#;
        let r: CallResult = serde_json::from_str(j).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content[0].text, "hi");
    }

    #[test]
    fn sanitize_works() {
        let input = "Error with sk-abc123def and Bearer xyz789";
        let out = sanitize_error(input);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("sk-abc123def"));
        assert!(!out.contains("Bearer xyz789"));
    }

    #[test]
    fn env_filter_blocks_secrets() {
        std::env::set_var("MY_SECRET_KEY", "leak-me");
        let env = build_safe_env(None);
        assert!(env.contains_key("PATH"));
        assert!(!env.contains_key("MY_SECRET_KEY"));
    }

    #[test]
    fn env_filter_allows_user_env() {
        let mut user = HashMap::new();
        user.insert("MCP_TOKEN".into(), "abc".into());
        let env = build_safe_env(Some(&user));
        assert_eq!(env.get("MCP_TOKEN").map(|s| s.as_str()), Some("abc"));
    }
}
