//! boxingAgent ACP（Agent Client Protocol）服务端。
//!
//! 实现 ACP JSON-RPC 2.0 over stdio，让 IDE（VS Code / Zed / JetBrains）
//! 通过标准输入/输出与 boxingAgent 通信。
//!
//! 协议方法（最小可用集）：
//! - initialize — 握手（协议版本 + 能力）
//! - session/new — 创建会话（cwd + session_id）
//! - session/prompt — 发送 prompt，流式返回 session/update 事件
//! - session/cancel — 取消正在运行的 prompt
//! - session/list — 列出会话
//!
//! 与 Python `acp_adapter/` 对等（最小集），edit approval / MCP per-session
//! / fork 等高级功能推迟。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{json, Value};

const ACP_PROTOCOL_VERSION: u32 = 1;

// ===== JSON-RPC =====

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

// ===== 会话 =====

struct AcpSession {
    session_id: String,
    cwd: PathBuf,
    model: String,
    system: String,
    max_turns: usize,
    max_tokens: u32,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

// ===== ACP 服务端 =====

/// ACP stdio 服务端：读取 stdin JSON-RPC，写入 stdout 响应/通知。
pub struct AcpServer {
    sessions: Mutex<HashMap<String, AcpSession>>,
    next_session: std::sync::atomic::AtomicU64,
}

impl AcpServer {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_session: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// 运行 ACP 服务端（阻塞，直到 stdin 关闭）。
    pub async fn run(self) -> anyhow::Result<()> {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let stdout = std::io::stdout();

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // stdin 关闭
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let req: RpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    let mut out = stdout.lock();
                    Self::write_error(&mut out, &Value::Null, -32700, &format!("解析错误: {e}"));
                    continue;
                }
            };

            let id = req.id.clone();
            let result = self.handle_method(&req).await;

            match result {
                Ok(Some(value)) => {
                    let mut out = stdout.lock();
                    Self::write_response(&mut out, &id.unwrap_or(Value::Null), &value);
                }
                Ok(None) => {
                    // 通知（无响应）
                }
                Err((code, msg)) => {
                    let mut out = stdout.lock();
                    Self::write_error(&mut out, &id.unwrap_or(Value::Null), code, &msg);
                }
            }
        }

        Ok(())
    }

    /// 处理 JSON-RPC 方法。
    async fn handle_method(&self, req: &RpcRequest) -> Result<Option<Value>, (i32, String)> {
        match req.method.as_str() {
            "initialize" => Ok(Some(self.handle_initialize())),
            "session/new" => Ok(Some(self.handle_new_session(&req.params)?)),
            "session/load" => Ok(Some(self.handle_load_session(&req.params)?)),
            "session/resume" => Ok(Some(self.handle_resume_session(&req.params)?)),
            "session/prompt" => Ok(Some(self.handle_prompt(&req.params).await?)),
            "session/cancel" => {
                self.handle_cancel(&req.params)?;
                Ok(Some(json!({})))
            }
            "session/list" => Ok(Some(self.handle_list_sessions())),
            _ => Err((-32601, format!("未知方法: {}", req.method))),
        }
    }

    // ===== 方法实现 =====

    fn handle_initialize(&self) -> Value {
        json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": true,
                "cancelPrompt": true,
            },
            "serverInfo": {
                "name": "boxingAgent",
                "version": "0.1.0",
            },
        })
    }

    fn handle_new_session(&self, params: &Value) -> Result<Value, (i32, String)> {
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("mimo-v2.5-pro");

        let session_id = format!(
            "boxing-{}",
            self.next_session.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );

        let session = AcpSession {
            session_id: session_id.clone(),
            cwd: PathBuf::from(cwd),
            model: model.to_string(),
            system: String::new(),
            max_turns: 30,
            max_tokens: 4096,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        self.sessions.lock().unwrap().insert(session_id.clone(), session);

        Ok(json!({
            "sessionId": session_id,
            "models": [{"id": model, "name": model}],
            "modes": [{"name": "default", "kind": "primary"}],
        }))
    }

    /// session/load：加载已有会话（从 state.db 恢复历史）。
    ///
    /// 与 Python `load_session` 对等：
    /// - 如果 session_id 存在于内存 → 更新 cwd
    /// - 如果不存在 → 从 state.db 查找（跨进程恢复）
    /// - 回放历史消息作为 session/update 通知
    fn handle_load_session(&self, params: &Value) -> Result<Value, (i32, String)> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "缺少 sessionId".to_string()))?;

        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        // 1. 检查内存中的会话
        let found = {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(session) = sessions.get_mut(session_id) {
                session.cwd = PathBuf::from(cwd);
                true
            } else {
                false
            }
        };

        if !found {
            // 2. 尝试从 state.db 恢复（跨进程持久化会话）
            let model = self
                .try_load_from_state_db(session_id)
                .map_err(|e| (-32603, format!("加载会话失败: {e}")))?;

            let model = model.unwrap_or_else(|| "mimo-v2.5-pro".to_string());
            let session = AcpSession {
                session_id: session_id.to_string(),
                cwd: PathBuf::from(cwd),
                model,
                system: String::new(),
                max_turns: 30,
                max_tokens: 4096,
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            self.sessions.lock().unwrap().insert(session_id.to_string(), session);
        }

        // 3. 回放历史（best-effort，从 state.db 读取）
        let _ = self.replay_history(session_id);

        // 4. 返回会话信息
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id).ok_or((-32602, "会话恢复失败".to_string()))?;

        Ok(json!({
            "models": [{"id": session.model, "name": session.model}],
            "modes": [{"name": "default", "kind": "primary"}],
        }))
    }

    /// session/resume：恢复会话（与 load 类似，但不存在时创建新的）。
    fn handle_resume_session(&self, params: &Value) -> Result<Value, (i32, String)> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "缺少 sessionId".to_string()))?;

        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        // 检查内存
        let found = {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(session) = sessions.get_mut(session_id) {
                session.cwd = PathBuf::from(cwd);
                true
            } else {
                false
            }
        };

        if !found {
            // 从 state.db 恢复或创建新会话
            let model = self
                .try_load_from_state_db(session_id)
                .ok()
                .flatten()
                .unwrap_or_else(|| "mimo-v2.5-pro".to_string());

            let session = AcpSession {
                session_id: session_id.to_string(),
                cwd: PathBuf::from(cwd),
                model,
                system: String::new(),
                max_turns: 30,
                max_tokens: 4096,
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            self.sessions.lock().unwrap().insert(session_id.to_string(), session);
        }

        // 回放历史
        let _ = self.replay_history(session_id);

        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id).ok_or((-32602, "会话恢复失败".to_string()))?;

        Ok(json!({
            "sessionId": session_id,
            "models": [{"id": session.model, "name": session.model}],
            "modes": [{"name": "default", "kind": "primary"}],
        }))
    }

    /// 从 state.db 查找会话的 model（跨进程恢复）。
    fn try_load_from_state_db(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let db_path = boxing_config::state_db_path()?;
        if !db_path.exists() {
            return Ok(None);
        }
        let summaries = boxing_state::SessionStore::open(&db_path)?.session_summaries()?;
        Ok(summaries.into_iter().find(|s| s.id == session_id).and_then(|s| s.model))
    }

    /// 回放会话历史（从 state.db 读取消息，发送 session/update 通知）。
    fn replay_history(&self, session_id: &str) -> anyhow::Result<()> {
        let db_path = boxing_config::state_db_path()?;
        if !db_path.exists() {
            return Ok(());
        }

        // 直接查询 messages 表（只读）
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        let mut stmt = conn.prepare(
            "SELECT role, content FROM messages \
             WHERE session_id = ?1 AND content IS NOT NULL \
             ORDER BY timestamp ASC",
        )?;

        let messages: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?
            .flatten()
            .collect();

        for (role, content) in messages {
            let speaker = match role.as_str() {
                "user" => "👤",
                "assistant" => "🤖",
                _ => "🔧",
            };
            let preview: String = content.chars().take(200).collect();
            Self::send_notification(
                session_id,
                "history",
                &format!("{speaker} {preview}"),
            );
        }

        Ok(())
    }

    async fn handle_prompt(&self, params: &Value) -> Result<Value, (i32, String)> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "缺少 sessionId".to_string()))?;

        // 提取 prompt 文本
        let prompt_text = params
            .get("prompt")
            .and_then(|p| p.as_array())
            .and_then(|blocks| {
                blocks.iter().find_map(|b| {
                    if b.get("type")? == "text" {
                        b.get("text")?.as_str().map(String::from)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default();

        if prompt_text.is_empty() {
            return Ok(json!({"stopReason": "end_turn"}));
        }

        // 取出 session 信息（需要 clone 出来，因为 agent.run 需要 &mut）
        let (model, system, max_turns, max_tokens, cancel) = {
            let sessions = self.sessions.lock().unwrap();
            let session = sessions
                .get(session_id)
                .ok_or((-32602, format!("会话不存在: {session_id}")))?;
            (
                session.model.clone(),
                session.system.clone(),
                session.max_turns,
                session.max_tokens,
                Arc::clone(&session.cancel),
            )
        };

        // 解析 provider + 构建 agent
        let config_path = boxing_config::config_path()
            .map_err(|e| (-32603, format!("config 路径错误: {e}")))?;
        let env_path = boxing_config::env_path()
            .map_err(|e| (-32603, format!("env 路径错误: {e}")))?;
        let config = boxing_config::load(&config_path)
            .map_err(|e| (-32603, format!("加载配置失败: {e}")))?;

        let provider = boxing_providers::resolve(&config, &env_path)
            .map_err(|e| (-32603, format!("解析 provider 失败: {e}")))?;
        let provider = Arc::from(provider);

        let tools = crate::agent_tools(
            Arc::clone(&provider),
            &model,
            &system,
            max_turns,
            max_tokens,
            &config,
        );

        let mut agent =
            boxing_core::Agent::new(provider, model, system, tools, max_turns, max_tokens);

        // 运行 agent loop
        let result = agent
            .run(&prompt_text, &mut |delta| {
                Self::send_notification(session_id, "agent_text_chunk", delta);
            }, &mut |event| {
                let (update_type, text) = match &event {
                    boxing_core::LoopEvent::ToolCall { name } => ("tool_call", format!("→ {name}")),
                    boxing_core::LoopEvent::ToolResult { name, ok } => {
                        let mark = if *ok { "✓" } else { "✗" };
                        ("tool_result", format!("{mark} {name}"))
                    }
                    boxing_core::LoopEvent::MaxTurns => ("max_turns", "达到最大轮数".to_string()),
                };
                Self::send_notification(session_id, update_type, &text);
            })
            .await;

        match result {
            Ok(text) => {
                // 检查是否被取消
                let cancelled = cancel.load(std::sync::atomic::Ordering::Relaxed);
                let stop_reason = if cancelled { "cancelled" } else { "end_turn" };
                Ok(json!({
                    "stopReason": stop_reason,
                    "response": [{"type": "text", "text": text}],
                }))
            }
            Err(e) => Err((-32603, format!("agent 运行失败: {e}"))),
        }
    }

    fn handle_cancel(&self, params: &Value) -> Result<(), (i32, String)> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "缺少 sessionId".to_string()))?;

        let sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(session_id) {
            session
                .cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    fn handle_list_sessions(&self) -> Value {
        let sessions = self.sessions.lock().unwrap();
        let infos: Vec<Value> = sessions
            .values()
            .map(|s| {
                json!({
                    "sessionId": s.session_id,
                    "cwd": s.cwd.display().to_string(),
                    "model": s.model,
                })
            })
            .collect();

        json!({
            "sessions": infos,
        })
    }

    // ===== 输出辅助 =====

    fn write_response(out: &mut std::io::StdoutLock, id: &Value, result: &Value) {
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let _ = writeln!(out, "{}", response);
        let _ = out.flush();
    }

    fn write_error(out: &mut std::io::StdoutLock, id: &Value, code: i32, message: &str) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        });
        let _ = writeln!(out, "{}", response);
        let _ = out.flush();
    }

    /// 发送 session/update 通知（不阻塞调用方）。
    fn send_notification(session_id: &str, update_type: &str, text: &str) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {"type": update_type, "text": text}
            }
        });
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(out, "{}", serde_json::to_string(&notification).unwrap_or_default());
        let _ = out.flush();
    }
}

/// 启动 ACP 服务端（从 boxing-agent acp 子命令调用）。
pub async fn run_acp_server() -> anyhow::Result<()> {
    let server = AcpServer::new();
    server.run().await
}
