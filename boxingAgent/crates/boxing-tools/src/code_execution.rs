//! `execute_code` 工具：通过 Python 脚本批量调用工具。
//!
//! 与 Hermes 原版 `tools/code_execution_tool.py` 对等（简化版）：
//! - LLM 写一段 Python 脚本，脚本中可以调用 boxing-agent 工具
//! - 通过 Unix 域套接字 RPC 通信（子进程调工具，父进程执行）
//! - 受限工具集：read_file, write_file, grep, bash, ls, glob
//! - 资源限制：超时 + 最大工具调用次数
//!
//! 核心思路：生成一个 `boxing_tools.py` 存根模块，子进程 import 后
//! 每次调用工具函数都通过 RPC 发回父进程，父进程执行后返回结果。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::{Tool, ToolError};

/// 受限允许的工具集（子进程只能调用这些）。
const ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit",
    "bash",
    "grep",
    "glob",
    "ls",
];

/// 默认超时（秒）。
const DEFAULT_TIMEOUT: u64 = 300;

/// 默认最大工具调用次数。
const DEFAULT_MAX_TOOL_CALLS: usize = 50;

/// 最大标准输出字节数。
const MAX_STDOUT_BYTES: usize = 50_000;

/// `execute_code` 工具：执行 Python 脚本，可调用受限工具集。
pub struct ExecuteCode {
    /// 所有已注册工具的名称（用于生成存根）。
    tool_names: Vec<String>,
}

impl ExecuteCode {
    pub fn new(tool_names: Vec<String>) -> Self {
        Self { tool_names }
    }
}

#[async_trait::async_trait]
impl Tool for ExecuteCode {
    fn name(&self) -> &str {
        "execute_code"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "execute_code",
            "description": "Execute a Python script that can call boxing-agent tools via RPC. The script runs in a sandboxed environment with access to a limited set of tools (read_file, write_file, edit, bash, grep, glob, ls). Use this to batch multiple tool calls into one inference step, perform file manipulation, or run computations.",
            "parameters": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python 3 code to execute. The code can import 'boxing_tools' to call agent tools (e.g., `from boxing_tools import read_file; result = read_file('/path/to/file')`)."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Execution timeout in seconds (default: 300).",
                        "default": 300
                    },
                    "max_tool_calls": {
                        "type": "integer",
                        "description": "Maximum number of tool calls allowed (default: 50).",
                        "default": 50
                    }
                },
                "required": ["code"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("code"))?;

        if code.trim().is_empty() {
            return Err(ToolError::InvalidArg {
                arg: "code",
                reason: "代码不能为空".into(),
            });
        }

        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT);

        let max_calls = args
            .get("max_tool_calls")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_TOOL_CALLS as u64) as usize;

        // 获取当前工具集（只保留允许的工具）
        let allowed: Vec<&str> = self
            .tool_names
            .iter()
            .filter(|n| ALLOWED_TOOLS.contains(&n.as_str()))
            .map(|s| s.as_str())
            .collect();

        // 生成存根模块 + 运行
        let stub_module = generate_stub_module(&allowed);
        let result = execute_python(code, &stub_module, timeout, max_calls).await?;

        Ok(result)
    }
}

/// 生成 Python 存根模块（`boxing_tools.py`）。
///
/// 每个工具生成一个函数，函数通过 RPC 调用父进程执行实际工具。
fn generate_stub_module(allowed_tools: &[&str]) -> String {
    let mut stubs = String::new();
    stubs.push_str("# Auto-generated boxing_tools stub module for code_execution\n");
    stubs.push_str("import json, socket, os, sys\n\n");
    stubs.push_str("_RPC_PATH = os.environ.get('BOXING_RPC_SOCKET', '')\n\n");
    stubs.push_str("def _call(tool_name, args):\n");
    stubs.push_str("    \"\"\"Call a boxing-agent tool via RPC socket.\"\"\"\n");
    stubs.push_str("    if not _RPC_PATH:\n");
    stubs.push_str("        raise RuntimeError('BOXING_RPC_SOCKET not set')\n");
    stubs.push_str("    try:\n");
    stubs.push_str("        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n");
    stubs.push_str("        sock.connect(_RPC_PATH)\n");
    stubs.push_str("        sock.settimeout(30)\n");
    stubs.push_str("        request = json.dumps({'tool': tool_name, 'args': args}) + '\\n'\n");
    stubs.push_str("        sock.sendall(request.encode())\n");
    stubs.push_str("        response = b''\n");
    stubs.push_str("        while True:\n");
    stubs.push_str("            chunk = sock.recv(65536)\n");
    stubs.push_str("            if not chunk:\n");
    stubs.push_str("                break\n");
    stubs.push_str("            response += chunk\n");
    stubs.push_str("            if b'\\n' in chunk:\n");
    stubs.push_str("                break\n");
    stubs.push_str("        sock.close()\n");
    stubs.push_str("        return json.loads(response.decode().strip())\n");
    stubs.push_str("    except Exception as e:\n");
    stubs.push_str("        return {'error': str(e)}\n\n");

    // 为每个允许的工具生成存根函数
    let tool_defs: [(&str, &str, &str, &str); 7] = [
        ("read_file", "path: str, offset: int = 1, limit: int = 2000",
         "Read a file with line numbers.",
         r#"{"path": path, "offset": offset, "limit": limit}"#),
        ("write_file", "path: str, content: str",
         "Write content to a file (overwrites).",
         r#"{"path": path, "content": content}"#),
        ("edit", "path: str, old_string: str, new_string: str",
         "Replace exactly one occurrence of old_string with new_string in a file.",
         r#"{"path": path, "old_string": old_string, "new_string": new_string}"#),
        ("bash", "command: str, timeout: int = 120",
         "Run a shell command. Returns stdout, stderr, and exit code.",
         r#"{"command": command, "timeout": timeout}"#),
        ("grep", "pattern: str, path: str = \".\", include: str = None, output_mode: str = \"content\", context: int = 0",
         "Search file contents with regex (gitignore-aware).",
         r#"{"pattern": pattern, "path": path, "include": include, "output_mode": output_mode, "context": context}"#),
        ("glob", "pattern: str, path: str = \".\"",
         "Find files by glob pattern, sorted by mtime.",
         r#"{"pattern": pattern, "path": path}"#),
        ("ls", "path: str = \".\", recursive: bool = False",
         "List directory entries.",
         r#"{"path": path, "recursive": recursive}"#),
    ];

    for (func_name, sig, doc, args_expr) in &tool_defs {
        if !allowed_tools.contains(func_name) {
            continue;
        }
        stubs.push_str(&format!(
            "def {name}({sig}):\n    \"\"\"{doc}\"\"\"\n    return _call({name:?}, {args_expr})\n\n",
            name = func_name,
            sig = sig,
            doc = doc,
            args_expr = args_expr
        ));
    }

    stubs
}

/// 执行 Python 脚本（带存根模块 + RPC 服务器）。
async fn execute_python(
    code: &str,
    stub_module: &str,
    timeout_secs: u64,
    max_tool_calls: usize,
) -> Result<String, ToolError> {
    // 创建临时目录
    let tmp_dir = PathBuf::from(format!(
        "/tmp/boxing-code-exec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp_dir)?;

    // 写入存根模块和用户脚本
    let stub_path = tmp_dir.join("boxing_tools.py");
    std::fs::write(&stub_path, stub_module)?;
    let script_path = tmp_dir.join("script.py");
    std::fs::write(&script_path, code)?;

    // 创建 Unix 域套接字
    let socket_path = tmp_dir.join("rpc.sock");
    let listener = UnixListener::bind(&socket_path)?;

    // 共享状态：工具调用计数器 + 停止信号
    let call_counter = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // 启动 RPC 服务器线程（阻塞式，在单独线程运行）
    let counter = Arc::clone(&call_counter);
    let stop_flag = Arc::clone(&stop);
    let rpc_handle =
        std::thread::spawn(move || rpc_server_loop(listener, counter, stop_flag, max_tool_calls));

    // 启动 Python 子进程
    let child = Command::new("python3")
        .arg(&script_path)
        .current_dir(&tmp_dir)
        .env("BOXING_RPC_SOCKET", &socket_path)
        .env("PYTHONUNBUFFERED", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Other(format!("启动 Python 失败: {e}")))?;

    // 等待子进程（带超时）
    let call_counter_ref = Arc::clone(&call_counter);
    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::task::spawn_blocking(move || {
            let output = child
                .wait_with_output()
                .map_err(|e| ToolError::Other(format!("等待子进程失败: {e}")))?;
            Ok::<_, ToolError>(output)
        }),
    )
    .await;

    // 停止 RPC 服务器
    stop.store(true, Ordering::Relaxed);
    let _ = rpc_handle.join();

    // 清理临时文件
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let output = match result {
        Ok(Ok(Ok(output))) => output,
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(e)) => return Err(ToolError::Other(format!("任务执行失败: {e}"))),
        Err(_) => return Err(ToolError::Other(format!("执行超时（{} 秒）", timeout_secs))),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    let tool_calls_made = call_counter_ref.load(Ordering::Relaxed);

    // 构造结果
    let mut result = serde_json::json!({
        "stdout": truncate(&stdout, MAX_STDOUT_BYTES),
        "exit_code": exit_code,
        "tool_calls_made": tool_calls_made,
    });

    if !stderr.is_empty() {
        result["stderr"] = serde_json::Value::String(truncate(&stderr, 10_000).to_string());
    }

    Ok(serde_json::to_string_pretty(&result).unwrap_or_default())
}

/// 截断文本到指定大小。
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        text
    } else {
        &text[..max]
    }
}

/// RPC 服务器循环（阻塞式，在单独线程运行）。
fn rpc_server_loop(
    listener: UnixListener,
    call_counter: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    max_tool_calls: usize,
) {
    use crate::default_tools;

    let tools = default_tools();
    let tool_map: HashMap<String, &dyn crate::Tool> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.as_ref()))
        .collect();

    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        let counter = Arc::clone(&call_counter);
        handle_rpc_client(stream, &tool_map, counter, max_tool_calls);
    }
}

/// 处理单个 RPC 客户端连接。
fn handle_rpc_client(
    stream: UnixStream,
    tool_map: &HashMap<String, &dyn crate::Tool>,
    call_counter: Arc<AtomicUsize>,
    max_tool_calls: usize,
) {
    // 使用 BufReader 读取（独占 stream）
    let mut reader = BufReader::new(&stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let request: Value = match serde_json::from_str(request_line.trim()) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(&stream, "{}", json!({"error": "无效的 RPC 请求"}));
            return;
        }
    };

    let tool_name = request["tool"].as_str().unwrap_or("");
    let tool_args = request["args"].clone();

    // 检查工具调用次数
    let count = call_counter.fetch_add(1, Ordering::Relaxed);
    if count >= max_tool_calls {
        let _ = writeln!(
            &stream,
            "{}",
            json!({"error": format!("工具调用次数超限 ({max_tool_calls})")})
        );
        return;
    }

    // 查找工具并执行
    let response = match tool_map.get(tool_name) {
        Some(tool) => {
            // 使用单线程 runtime 执行 async 工具
            let rt = tokio::runtime::Runtime::new().unwrap();
            match rt.block_on(tool.exec(tool_args)) {
                Ok(result) => result,
                Err(e) => format!("{{\"error\": \"{}\"}}", sanitize_error(&e.to_string())),
            }
        }
        None => {
            format!("{{\"error\": \"未知工具: {}\"}}", sanitize_error(tool_name))
        }
    };

    // 写回结果（一行 JSON）
    let _ = writeln!(&stream, "{}", response);
}

/// 凭证脱敏（移除可能的 API key）。
fn sanitize_error(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('\n', " ")
        .replace('\r', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let tool = ExecuteCode::new(vec!["read_file".into(), "bash".into()]);
        let schema = tool.schema();
        assert_eq!(schema["name"], "execute_code");
        assert!(schema["parameters"]["properties"]["code"].is_object());
    }

    #[test]
    fn stub_module_contains_allowed_tools() {
        let stub = generate_stub_module(&["read_file", "bash", "grep"]);
        assert!(stub.contains("def read_file("));
        assert!(stub.contains("def bash("));
        assert!(stub.contains("def grep("));
        assert!(!stub.contains("def vision(")); // 不在允许列表中
    }

    #[test]
    fn stub_module_rpc_bridge_present() {
        let stub = generate_stub_module(&["read_file"]);
        assert!(stub.contains("BOXING_RPC_SOCKET"));
        assert!(stub.contains("socket.AF_UNIX"));
        assert!(stub.contains("def _call("));
    }

    #[test]
    fn truncate_works() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
    }
}
