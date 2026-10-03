# 花火角色 Agent：第一阶段

这是 Hermes 源码中的独立扩展，服务于电脑端双 Link 2C 感知、Godot 实景角色与大屏交互。Hermes 负责角色回复与会话，场景调度器负责角色接话和回合，Godot 负责动作执行。当前提供源码 CLI、HTTP 适配器和可选 Godot 消费端；没有替换现有语音服务或连接正在运行的游戏。

上游基线为 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)，固定提交 `bd0affe5e5f723579df8902852f5d0c47795f355`。项目分支为 [codex/huahuo-character-runtime](https://github.com/zhaojiaxiang370711/hermes-agent/tree/codex/huahuo-character-runtime)，保留上游历史、MIT 许可证和原始署名。角色素材的权利与软件许可证分别处理。

## 已有能力

- 每个角色一个 `CharacterSpec` 和独立 Hermes profile 引用，实际 profile 尚需创建；对话 session 由角色、用户和场景共同派生。
- 同一用户、同一场景串行接话；默认最多再接话一次，配置范围 0–4 次，避免角色无限互聊。只转发公开台词，超长转发片段会明确标记截断。
- 新回合或打断会使旧输出失效；调用方取消同样生效。动作的身份、回合与请求 ID 由调度器生成，模型只能选择台词、伙伴和允许的语义动作。
- 本机 Hermes `/p/<profile>/v1/runs` 适配：按 profile 鉴权、稳定 session、提交幂等键、有限轮询，取消或超时尝试 stop。仅完整终态回复可执行，部分输出和审批请求被拒绝。
- 默认关闭的 Godot 桥接：严格消息字段、目标绑定、回合校验和去重；固定动作优先级 60，忙时拒绝，不积压稍后执行。现有抓取和拍照保持优先。

示例使用派蒙与可莉两个角色身份。可莉的动作能力设为空，因为当前地面程序步态不等于派蒙的动作库；示例回复不代表已经驱动两个角色资产。

## 本地离线运行

在仓库根目录使用 Python 3.12 或以上，不需要第三方运行依赖、模型密钥或摄像头：

```bash
python3 -m huahuo_character demo
python3 -m huahuo_character demo --character klee --user demo-user --conversation room-1 --text '和派蒙聊聊吧'
python3 -m huahuo_character export-profiles --output /tmp/huahuo-profile-bundle
```

默认 backend 为 `mock`，台词有“离线模拟”标记。`export-profiles` 只在一个全新目录生成 `SOUL.md` 与清单；已有目录会拒绝覆盖，不会安装 profile、读取密钥或修改现有 Hermes home。

CLI 每次执行都会新建 Director，回合从 1 开始，只用于演示。长期宿主需要复用同一个 Director，并在重启时先使 Godot 旧作用域失效；当前尚无场景状态持久化和空闲状态回收。互聊链完成后统一返回，多个 run 的等待预算会累计，尚未提供流式语音输出。

## 本机 Hermes 接口

适配器针对上述固定上游提交核对，旧版本可能缺少持久会话续接和多 profile 路由。完整 Hermes 运行环境应按上游 [安装文档](../website/docs/getting-started/installation.md) 和 [PM 工作流](../website/docs/reference/package-management.md) 准备；本扩展没有自动安装或启动它。

准备两个独立 profile 并导入生成的角色设定后，使用上游 [多 profile 网关](../website/docs/user-guide/multi-profile-gateways.md) 与 [HTTP API](../website/docs/user-guide/features/api-server.md)。CLI 从 `HUAHUO_HERMES_API_TOKENS` 环境变量接收 JSON 映射，必须恰好覆盖配置中的 profile，例如 `{"huahuo-paimon":"<派蒙本机网关密钥>","huahuo-klee":"<可莉本机网关密钥>"}`。它们是各 profile 的 `API_SERVER_KEY`，不是模型供应商的 API key。宿主注入该变量后运行：

```bash
python3 -m huahuo_character demo --backend hermes --hermes-url http://127.0.0.1:8642
```

Python 宿主也可直接构造 `HermesBackend(url, api_tokens={profile: token, ...})`。每个 profile 请求始终走自己的前缀、token 和 session。URL 仅允许回环地址，拒绝重定向并绕过环境代理。错误输出省略服务响应正文与密钥。

JSON 输出约束只限制回复，不能替代 Hermes 工具授权。接现场用户前应给角色 profile 配置最小工具集，尤其不能默认授予终端、文件、跨会话搜索或外发能力。角色动画通过本扩展的受限消息执行，而非模型生成脚本。

在此上游版本，`platform_toolsets.api_server: []` 仍可能加入全局 MCP 与默认插件；`[no_mcp]` 可禁止 MCP 自动加入，但仍需核对插件、xAI 和 context engine 的最终工具列表。本次没有安装任何角色 profile 或开放这些工具。

stop 是停止请求，返回 `stopping` 并不表示远端已退出。远端同 session 写入串行依赖 Hermes 的 durable lease；本地锁只串行本地适配器任务。若提交响应丢失或 stop 失败，本层不能保证终止远端计算，也不能回滚它已执行的工具；但旧回合结果仍会被本地调度器拒绝。幂等键已发送，自动重试和重连恢复尚未实现。

`X-Hermes-Session-Key` 为支持该机制的 memory provider 提供稳定作用域；各用户 transcript 在本层分开。profile 下的 `MEMORY.md`、`USER.md` 等资料及工具并不会因为 transcript 不同自动隔离。关系记忆、跨场景记忆策略和用户删除机制属于下一阶段，尚未验收。

## 动作协议与 Godot

模型回复只允许以下字段：

```json
{"text":"你好，我们一起玩吧。","action":"greet","target_character_id":"klee"}
```

调度器生成 `huahuo.character.action.v1` 消息，包含 `type`、`schema`、`request_id`、`character_id`、`conversation_id`、`turn_id`、`action`。不接受模型提供的动画路径、代码、原始 clip 名或优先级。

| 语义动作 | 派蒙 clip |
| --- | --- |
| greet | introduction |
| explain | guiding |
| celebrate | excited |
| approve | thumbs_up |
| think | thinking |
| shrug | shrug |
| apologize | facepalm |
| surprise | surprised_lean |

宿主为每个支持此动作库的角色建立一个 [桥接实例](godot/character_action_bridge.gd)，绑定现有 `moment(clip, now, priority)` 裁决器，再显式启用。宿主应在输入开始时调用 `begin_scope`，使用自己保存的权威回合；不能用到达事件中的 ID 自动建立作用域。打断、断连、切角色及重启时调用 `invalidate`，同时由宿主停止语音并处理已开始动作。

`started` 回执表示裁决器允许启动，不表示动作播放完成；`invalidate` 只使后续请求失效，不重置抓取、拍照或强行停止已开始动画。同优先级动作仍由现有裁决器决定是否替换。未来需增加播放完成、取消与失败回执，以及语音和动作的时间同步。现有情绪动作通道也需统一，避免同一台词重复触发。

## 验证

Python 检查用上游统一 wrapper，独立测试解释器中只需固定版本 `pytest==9.0.2`；`--confcutdir` 将无 Hermes 核心依赖的扩展测试与核心 fixture 分开：

```bash
HERMES_PYTHON="$PWD/.venv/bin/python" scripts/run_tests.sh tests/huahuo_character -j 2 -- -c huahuo_character/pytest.ini --confcutdir="$PWD/tests/huahuo_character" -q
godot --headless --path huahuo_character/godot --script res://test_character_action_bridge.gd
python3 -m huahuo_character demo > /tmp/huahuo-character-demo.json
godot --headless --path huahuo_character/godot --script res://test_paimon_action_integration.gd -- --paimon-actions-path /path/to/game-101/paimon_actions.gd --fixture /tmp/huahuo-character-demo.json
```

HTTP 测试使用真实本机测试服务器；Godot 检查使用合成 AnimationPlayer，并可加载现有游戏的真实派蒙裁决脚本。结果证明协议、取消和动作优先级边界，不证明真实模型回复质量、摄像头感知、资产播放观感、语音延迟或真人现场验收。项目进展见 [后续阶段](ROADMAP.md)。
