# 花火角色 Agent：常驻服务与语音接入

这是 Hermes 源码中的独立扩展，服务于电脑端双 Link 2C 感知、Godot 实景角色与大屏交互。Hermes 负责角色回复与会话，场景 Director 负责角色接话和回合，现有 Python 语音网关负责 ASR、CosyVoice TTS 和 WebSocket，Godot 负责收音、播报与动作执行。当前已有常驻角色服务及主工程语音接入代码；真实文本双角色语音、合成 WAV 经真实 ASR 的部署检查已通过，真人双摄交互仍待验收。

上游基线为 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)，固定提交 `bd0affe5e5f723579df8902852f5d0c47795f355`。项目分支为 [codex/huahuo-character-runtime](https://github.com/zhaojiaxiang370711/hermes-agent/tree/codex/huahuo-character-runtime)，保留上游历史、MIT 许可证和原始署名。角色素材的权利与软件许可证分别处理。

## 已有能力

- 每个角色一个 `CharacterSpec` 和独立 Hermes profile 引用；当前部署已有派蒙、可莉两个 profile。对话 session 由角色、用户和场景共同派生，新环境仍需单独创建并配置 profile。
- 同一用户、同一场景串行接话；默认最多再接话一次，配置范围 0–4 次，避免角色无限互聊。只转发公开台词，超长转发片段会明确标记截断。
- 新回合或打断会使旧输出失效；调用方取消同样生效。动作的身份、回合与请求 ID 由调度器生成，模型只能选择台词、伙伴和允许的语义动作。
- 本机 Hermes `/p/<profile>/v1/runs` 适配：按 profile 鉴权、稳定 session、提交幂等键、有限轮询，取消或超时尝试 stop。仅完整终态回复可执行，部分输出和审批请求被拒绝。
- 常驻 [HTTP 服务](server.py) 复用同一个 Director；新进程改变公开播放 scope，Hermes transcript session 保持稳定。服务提供鉴权轮次和打断接口、输入限额、请求超时及客户端断连取消。
- 台词携带宿主生成的可选 `action_request_id`，精确关联对应动作；角色重复接话且某句没有动作时，不会借用同角色其他句子的动作。
- Godot 桥接严格检查字段、目标、回合和重复请求；固定动作优先级 60，忙时拒绝，不积压稍后执行。独立桥接默认关闭，主工程由宿主显式绑定派蒙现有动作裁决器；抓取和拍照保持优先。

示例使用派蒙与可莉两个角色身份。可莉的动作能力设为空，因为当前地面程序步态不等于派蒙的动作库。两角色目前共用同一个 CosyVoice 音色，输出元数据明确标记共享音色；不同字幕身份不代表已有不同角色声音。

## 本地离线运行

在仓库根目录使用 Python 3.12 或以上，不需要第三方运行依赖、模型密钥或摄像头：

```bash
python3 -m huahuo_character demo
python3 -m huahuo_character demo --character klee --user demo-user --conversation room-1 --text '和派蒙聊聊吧'
python3 -m huahuo_character export-profiles --output /tmp/huahuo-profile-bundle
```

默认 backend 为 `mock`，台词有“离线模拟”标记。`export-profiles` 只在一个全新目录生成 `SOUL.md` 与清单；已有目录会拒绝覆盖，不会安装 profile、读取密钥或修改现有 Hermes home。

此 CLI 每次执行都会新建 Director，回合从 1 开始，只用于演示。长期运行应使用下述 `huahuo_character.server`。当前尚无场景状态持久化和空闲状态回收。

## 常驻角色服务

宿主生成一个全新的共享本机 bearer token，通过 `HUAHUO_CHARACTER_SERVER_TOKEN` 注入服务；通过 `HUAHUO_HERMES_API_TOKENS` 注入各 profile 的网关 token 映射。服务不会查找已有 `.env`、复制模型凭据或自行生成密钥文件。

```bash
python3 -m huahuo_character.server --host 127.0.0.1 --port 8643 --hermes-url http://127.0.0.1:8642
```

服务默认使用真实 Hermes backend。离线服务测试必须显式加 `--backend mock`，此时仍需要注入新的服务鉴权 token。`--config` 指定角色配置，`--request-timeout` 默认 60 秒。监听仅限回环地址，日志不输出 token、输入正文或远端错误正文。

| 接口 | 请求与返回 |
| --- | --- |
| `GET /health` | 无鉴权的非秘密元数据：ready、instance_id、backend、协议版本；ready 只表示本机服务就绪，不证明模型可用 |
| `POST /v1/turn` | Bearer 鉴权；精确字段 `character_id`、`user_id`、`conversation_id`、`text`；返回 TurnResult 及 instance_id |
| `POST /v1/interrupt` | Bearer 鉴权；精确字段 `user_id`、`conversation_id`；返回新的权威播放 scope 与 turn_id |

输入中的用户、场景和角色身份由可信宿主选定。返回的 `conversation_id` 是含进程实例的 opaque scope；动作使用同一 scope。进程重启后旧回合不会因 turn_id 重新从 1 开始而被重新接纳，Hermes 的稳定 transcript key 则继续用于恢复对话。宿主仍需在新输入、断连和停止时立即使本地旧播放 scope 失效。

## 本机 Hermes 接口

适配器针对上述固定上游提交核对，旧版本可能缺少持久会话续接和多 profile 路由。完整 Hermes 运行环境应按上游 [安装文档](../website/docs/getting-started/installation.md) 和 [PM 工作流](../website/docs/reference/package-management.md) 准备；本扩展没有自动安装或启动它。

准备两个独立 profile 并导入生成的角色设定后，使用上游 [多 profile 网关](../website/docs/user-guide/multi-profile-gateways.md) 与 [HTTP API](../website/docs/user-guide/features/api-server.md)。CLI 从 `HUAHUO_HERMES_API_TOKENS` 环境变量接收 JSON 映射，必须恰好覆盖配置中的 profile，例如 `{"huahuo-paimon":"<派蒙本机网关密钥>","huahuo-klee":"<可莉本机网关密钥>"}`。它们是各 profile 的 `API_SERVER_KEY`，不是模型供应商的 API key。宿主注入该变量后运行：

```bash
python3 -m huahuo_character demo --backend hermes --hermes-url http://127.0.0.1:8642
```

Python 宿主也可直接构造 `HermesBackend(url, api_tokens={profile: token, ...})`。每个 profile 请求始终走自己的前缀、token 和 session。URL 仅允许回环地址，拒绝重定向并绕过环境代理。错误输出省略服务响应正文与密钥。

JSON 输出约束只限制回复，不能替代 Hermes 工具授权。接现场用户前应给角色 profile 配置最小工具集，尤其不能默认授予终端、文件、跨会话搜索或外发能力。角色动画通过本扩展的受限消息执行，而非模型生成脚本。

在此上游版本，`platform_toolsets.api_server: []` 仍可能加入全局 MCP 与默认插件；`[no_mcp]` 可禁止 MCP 自动加入，但仍需核对插件、xAI 和 context engine 的最终工具列表。当前两个角色 profile 使用 `api_server: [no_mcp]`、空 `toolsets`、compressor context engine，无插件或 xAI，关闭 lazy installs；已分别实测 `/v1/toolsets` 的 enabled 列表为空，本配置未开放终端、文件、跨会话搜索或外发能力。这是部署工具配置的结果，不是回复 JSON 契约提供的保证。

stop 是停止请求，返回 `stopping` 并不表示远端已退出。远端同 session 写入串行依赖 Hermes 的 durable lease；本地锁只串行本地适配器任务。若提交响应丢失或 stop 失败，本层不能保证终止远端计算，也不能回滚它已执行的工具；但旧回合结果仍会被本地调度器拒绝。幂等键已发送，自动重试和重连恢复尚未实现。

当前部署关闭普通 profile memory 与外部 memory provider，只保留各角色、用户和场景的 transcript 上下文，不宣称已有独立的用户关系记忆。`X-Hermes-Session-Key` 可为支持该机制的 memory provider 提供稳定作用域；若未来启用，profile 下的 `MEMORY.md`、`USER.md` 等资料及工具不会因为 transcript 不同自动隔离。关系记忆、跨场景记忆策略和用户删除机制属于后续阶段。

## 主工程语音链路

花火主工程的 `paimon-voice-poc/src/runtime/character_service.py` 调用此常驻服务，`pipeline.py` 将现有 ASR 结果交给 Hermes，再逐句调用 CosyVoice 并沿 WebSocket 输出 PCM。主工程 `godotplayer/game-101/voice_gateway_client.gd` 消费权威回合、角色字幕、音频与语义动作。视觉问答的观察作为参考数据送入 Hermes，由角色组织最终台词。

语音网关通过 `HUAHUO_CHARACTER_SERVICE_URL` 指定服务地址，通过 `HUAHUO_CHARACTER_SERVICE_TOKEN` 接收与服务端相同的新共享 token；角色与用户身份由网关宿主配置，客户端消息不能指定 Hermes profile。服务和网关 token 名称不同，部署宿主负责映射同一 secret。网关的 ASR/TTS 模型凭据保留在自己的配置边界内。

当前 Hermes 适配器等待每个 run 的完整终态，整个有界接话链结束后一次返回结果；没有接入 Hermes SSE 模型增量流。之后每句 TTS 流式输出，台词与动作靠 `action_request_id` 对应。多个 run 的等待预算会累计，不应把一轮总耗时当成模型首字或语音首音延迟。

新输入和取消先发送 `turn.invalidate`，返回结果后再使用服务的权威 scope 发送 `turn.begin`。网关按本地 generation 拒绝迟到回复，取消清理排空后才提交替代回合；音频头后紧跟对应 PCM。角色路径使用语义动作，不再为同一句回复额外触发旧情绪动作。当前网关为每条 WebSocket 连接生成场景 ID；同连接可续聊，重新连接不会自动恢复前一连接的 transcript，跨连接恢复策略尚待实现。

## 动作协议与 Godot

模型回复只允许以下字段：

```json
{"text":"你好，我们一起玩吧。","action":"greet","target_character_id":"klee"}
```

调度器生成 `huahuo.character.action.v1` 消息，包含 `type`、`schema`、`request_id`、`character_id`、`conversation_id`、`turn_id`、`action`。台词的可选 `action_request_id` 指向对应事件，不改变动作的七字段格式。不接受模型提供的关联 ID、动画路径、代码、原始 clip 名或优先级。

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

网关在对应台词的首个 PCM 输出前发送动作事件。这是生成和传输顺序同步，不是 Godot 扬声器开始播放的 ACK；客户端缓冲及设备延迟尚未被精确计入。`started` 回执表示裁决器允许启动，不表示动作播放完成；`invalidate` 只使后续请求失效，不重置抓取、拍照或强行停止已开始动画。同优先级动作仍由现有裁决器决定是否替换。未来需增加播放时间 ACK、动画完成/取消/失败回执和每角色音色。

## 验证

Python 检查用上游统一 wrapper，独立测试解释器中只需固定版本 `pytest==9.0.2`；`--confcutdir` 将无 Hermes 核心依赖的扩展测试与核心 fixture 分开：

```bash
HERMES_PYTHON="$PWD/.venv/bin/python" scripts/run_tests.sh tests/huahuo_character -j 2 -- -c huahuo_character/pytest.ini --confcutdir="$PWD/tests/huahuo_character" -q
godot --headless --path huahuo_character/godot --script res://test_character_action_bridge.gd
python3 -m huahuo_character demo > /tmp/huahuo-character-demo.json
godot --headless --path huahuo_character/godot --script res://test_paimon_action_integration.gd -- --paimon-actions-path /path/to/game-101/paimon_actions.gd --fixture /tmp/huahuo-character-demo.json
```

HTTP 自动测试使用真实本机测试服务器；Godot 自动检查使用合成 AnimationPlayer，并可加载现有游戏的真实派蒙裁决脚本。自动测试与下面的部署检查分别记录。

2026-10-03 已确认的检查：

| 检查 | 结果与证据 | 适用范围 |
| --- | --- | --- |
| 本扩展上游统一 Python wrapper | 55 项通过、0 失败、无 flaky | Director、动作关联、profile 路由、鉴权、常驻 HTTP、重启 scope、超时和取消；Hermes 网络测试使用本机测试服务器 |
| 已部署 Hermes → 双 profile → CosyVoice → WebSocket | 主工程 `output/hermes-voice-20261003/text.json`：461280 字节 PCM、65 对音频头/二进制块、可莉/派蒙两个身份、1 个动作；scope 与 complete 正确，0 错误，全轮 12.408 秒 | 真实模型和 TTS、文本输入、共享音色、WS 传输；12.408 秒是整轮耗时，不是首音延迟 |
| 合成 WAV → 真实 ASR → Hermes → CosyVoice → WebSocket | 主工程 `output/hermes-voice-20261003/asr.json`：16 kHz 单声道 PCM 输入，2 条 ASR final，派蒙回复 184800 字节 PCM、26 对音频头/二进制块；scope 与 complete 正确，0 错误，全轮 9.983 秒 | 使用合成音频，非真人麦克风；该轮只有派蒙回复，没有可莉接话或动作；耗时包含实时送入音频，不是首音延迟 |

部署证据属于花火主工程，未复制用户凭据或角色资产进入此 fork。上述结果不证明真人无线麦克风、外放回声、双摄感知、实际资产播放观感、整体 FPS 或现场可用性。项目进展见 [后续阶段](ROADMAP.md)。
