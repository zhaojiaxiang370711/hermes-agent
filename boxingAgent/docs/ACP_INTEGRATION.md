# boxingAgent ACP 集成指南

## 概述

boxingAgent 支持 **ACP (Agent Client Protocol)** — 一个基于 stdio JSON-RPC 2.0 的协议，让 IDE（VS Code / Zed / JetBrains）通过标准输入/输出与 boxingAgent 通信。

## 快速开始

```bash
# 启动 ACP 服务端
boxing-agent acp
```

服务端从 stdin 读取 JSON-RPC 请求，向 stdout 写入响应和通知。stderr 用于日志。

## 协议方法

### 1. initialize — 握手

```jsonc
// 请求
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}

// 响应
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion": 1,
  "agentCapabilities": {"loadSession": true, "cancelPrompt": true},
  "serverInfo": {"name": "boxingAgent", "version": "0.1.0"}
}}
```

### 2. session/new — 创建会话

```jsonc
// 请求
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","model":"mimo-v2.5-pro"}}

// 响应
{"jsonrpc":"2.0","id":2,"result":{
  "sessionId": "boxing-1",
  "models": [{"id":"mimo-v2.5-pro","name":"mimo-v2.5-pro"}],
  "modes": [{"name":"default","kind":"primary"}]
}}
```

### 3. session/prompt — 发送 prompt（流式输出）

```jsonc
// 请求
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{
  "sessionId": "boxing-1",
  "prompt": [{"type":"text","text":"列出当前目录的文件"}]
}}

// 流式通知（agent 文本 + 工具调用）
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId": "boxing-1",
  "update": {"type":"agent_text_chunk","text":"正在执行..."}
}}
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId": "boxing-1",
  "update": {"type":"tool_call","text":"→ bash"}
}}

// 最终响应
{"jsonrpc":"2.0","id":3,"result":{
  "stopReason": "end_turn",
  "response": [{"type":"text","text":"以下是文件列表..."}]
}}
```

### 4. session/cancel — 取消正在运行的 prompt

```jsonc
// 请求
{"jsonrpc":"2.0","id":4,"method":"session/cancel","params":{"sessionId":"boxing-1"}}

// 响应
{"jsonrpc":"2.0","id":4,"result":{}}

// 被取消的 prompt 最终返回
{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled","response":[]}}
```

### 5. session/load — 加载已有会话

```jsonc
{"jsonrpc":"2.0","id":5,"method":"session/load","params":{"sessionId":"boxing-1","cwd":"/tmp"}}

// 如果 state.db 有历史消息，会先发送回放通知：
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId": "boxing-1",
  "update": {"type":"history","text":"👤 之前的问题..."}
}}

// 响应
{"jsonrpc":"2.0","id":5,"result":{
  "models": [{"id":"mimo-v2.5-pro","name":"mimo-v2.5-pro"}],
  "modes": [{"name":"default","kind":"primary"}]
}}
```

### 6. session/resume — 恢复会话

与 load 类似，但会话不存在时创建新的。

### 7. session/list — 列出会话

```jsonc
{"jsonrpc":"2.0","id":6,"method":"session/list","params":{}}

{"jsonrpc":"2.0","id":6,"result":{
  "sessions": [{"sessionId":"boxing-1","cwd":"/tmp","model":"mimo-v2.5-pro"}]
}}
```

## session/update 事件类型

| type | 说明 |
|---|---|
| `agent_text_chunk` | 模型流式文本片段 |
| `tool_call` | 工具调用开始（→ 工具名） |
| `tool_result` | 工具执行结果（✓ 成功 / ✗ 失败） |
| `tool_approval` | 工具审批通知（write/edit/bash） |
| `max_turns` | 达到最大轮数 |
| `cancelled` | 会话被取消 |
| `history` | 历史消息回放 |

## IDE 集成配置

### VS Code（通过 acp-bridge）

```json
// settings.json
{
  "acp.servers": {
    "boxingAgent": {
      "command": "boxing-agent",
      "args": ["acp"]
    }
  }
}
```

### Zed

```json
// settings.json
{
  "agent_servers": {
    "boxingAgent": {
      "command": "boxing-agent",
      "args": ["acp"]
    }
  }
}
```

## 错误处理

```jsonc
// 未知方法
{"error":{"code":-32601,"message":"未知方法: foo"},"id":1,"jsonrpc":"2.0"}

// 缺少参数
{"error":{"code":-32602,"message":"缺少 sessionId"},"id":2,"jsonrpc":"2.0"}

// 运行时错误
{"error":{"code":-32603,"message":"agent 运行失败: ..."},"id":3,"jsonrpc":"2.0"}
```

## 当前限制

- **edit approval**: 当前为信息性通知（自动通过），双向 IDE 审批后续补
- **fork_session**: 未实现
- **set_session_model/mode**: 未实现（固定使用 session/new 时指定的 model）
- **MCP per-session**: 未实现（所有会话共享 config.yaml 中的 MCP 服务器）
