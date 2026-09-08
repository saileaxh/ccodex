# 行为一致性审计（基线：openai/codex 0.153.4 @ 1fb5158b）

审计口径：上游（chatgpt.com backend）能从每个 HTTP 请求观察到的所有内容——
请求头、请求体（zstd 解压后）、URL、TLS/HTTP 栈特征、时序行为。

## 一、下行→上行的总原则

**不透传**。下游客户端的任何字段/头都不直接进入上行请求。下行只提取语义内容
（model / instructions / input / tools / reasoning / text.format），上行请求由
`request_build.rs` 按官方 `build_responses_request`（core/src/client.rs）从头组装，
所有行为参数来自官方 models.json 与官方默认配置。

## 二、已验证一致（冒烟 + 单测覆盖）

### 请求头（/responses）
| 头 | 来源 | 说明 |
|---|---|---|
| authorization / chatgpt-account-id | 官方 auth_provider | Bearer + 账号 id，401 走官方刷新恢复 |
| originator / user-agent / version | 官方 default_client + Provider | UA 五段式，版本烘焙 0.153.4 |
| session-id / thread-id / x-client-request-id | request_build | **v7 UUID**（与官方 SessionId/ThreadId::new 同版本），同下游会话稳定复用 |
| x-codex-window-id | request_build | `{thread_id}:0`（官方主窗口格式） |
| x-codex-turn-metadata | request_build | 官方 CodexTurnMetadataPayload 全字段（见下） |
| x-openai-internal-codex-responses-lite | request_build | 仅 lite 模型 |
| accept / content-encoding | forward | text/event-stream / zstd（官方默认开启压缩） |

默认**不发送**（与官方默认配置一致）：x-codex-beta-features、x-codex-turn-state
（官方每个 turn 新建 ModelClientSession，首轮恒无此头）、x-codex-installation-id 头
（仅 compaction 路径）、x-openai-subagent（非子代理）、traceparent/tracestate
（otel 默认关闭）、conversation_id。

### 请求体（/responses）
字段集合与顺序 = 官方 `ResponsesApiRequest` 结构体序（serde_json preserve_order）：

- **lite 模型**（当前主力：gpt-6-astra / gpt-5.6-sol / gpt-5.6-terra / gpt-5.6-luna 等）：
  顶层无 instructions/tools；input 首两项为官方前缀项——
  `additional_tools`（id=`at_<v5(prefix_ns, tools)>`，role=developer，function/custom 收编进
  `{"type":"namespace","name":"functions","description":""}`，其余工具类型保持顶层相对顺序）
  + developer base-instructions message（id=`msg_<v5(prefix_ns, instructions)>`，
  `internal_chat_message_metadata_passthrough.content_item_kinds=["model.base_instructions"]`）；
  `parallel_tool_calls=false`；`reasoning.context="all_turns"`；image detail 剥离。
- **非 lite 模型**（gpt-5.5/5.4/5.4-mini/5.2）：顶层 instructions + tools；
  `parallel_tool_calls=true`；reasoning 无 context。
- **reasoning**：effort = 用户显式值 → 模型名后缀（gpt-x-high）→ 模型默认，
  经官方 resolve 映射（ultra→multi_agent/max/最后非 ultra；persistent→disabled）；
  summary = 用户显式值 → 模型默认（"none" 或不支持时省略，gpt-5.2 默认发 "auto"）。
- **text**：verbosity 用户显式值 → 模型默认（support_verbosity 时）；用户 output schema →
  官方 TextFormat 形状 `{type:"json_schema",strict,schema,name:"codex_output_schema"}`。
- **固定值**：`store:false`、`stream:true`、`include:["reasoning.encrypted_content"]`、
  `tool_choice:"auto"`、`prompt_cache_key=session_id`；service_tier/stream_options/access_programs
  官方默认 None → 省略。
- **client_metadata 六键**：x-codex-installation_id（**按账号目录持久化**，与官方
  `<codex_home>/installation_id` 同语义）、session_id、thread_id、x-codex-window-id、
  turn_id（**v7**，每请求新生成，换号重试不变）、x-codex-turn-metadata（与同名头同串）。
- **turn metadata payload**（官方默认 CLI 首轮）：installation_id/session_id/thread_id、
  agent_name="root"、turn_id、window_id/window_number=0、request_kind="turn"、
  thread_source="user"、sandbox="none"、sandbox_mode="read-only"（Windows 默认配置）、
  auto_review_enabled=false、node_repl_*（随模型）、turn_started_at_unix_ms。
- **item 规范化**：无前缀 item id 丢弃（官方 prepare_response_items_for_request）；
  无前缀保留规则 = `prefix_suffix` 双非空。
- 下游专有字段（temperature/max_output_tokens/previous_response_id/metadata 等）全部剥离。

### GET /models
只发 `client_version=<身份版本>` query（官方 client_version_to_whole），
下游 query 不透传；5s 超时与官方 MODELS_REFRESH_TIMEOUT 一致；无会话头（官方如此）。

### 传输与认证栈
官方 ReqwestTransport + default_client（Cloudflare cookies、系统代理含 socks）。
**每账号一个官方 client**（官方语义：一进程一账号一 client）——各账号独立的连接池
与 cookie jar，任何两个账号的请求绝不会出现在同一条 TLS/H2 连接上。
下行响应：SSE 字节流逐字节透传（零再序列化），配额头旁路解析记账。

### 多请求行为（跨请求一致性）
- 同会话多轮：session/thread id 稳定（SessionStore 记忆映射，TTL 24h），每轮新 v7
  turn_id、新 turn_started_at_unix_ms；HTTP 路径官方每轮新建 ModelClientSession，
  恒不带 turn-state——中继同样如此。
- prompt_cache_key=session_id 稳定 → 上游 prompt cache 命中行为与官方一致。
- **会话/turn id 按 (会话, 账号) 记忆**：同一下游会话换号即获得全新 v7 会话与全新
  turn_id——官方客户端的 session/turn id 永远不会出现在两个账号下，复用即构成
  跨账号关联信号。同账号内 id 稳定（缓存保留）；401 同账号刷新重试沿用同一
  turn（对齐官方 UnauthorizedRecovery）。
- 同账号内连接复用（H2 多路复用）与官方进程内行为一致；跨账号不复用连接（见上）。
- 中继重启 / 会话 TTL 过期后同下游会话获得新 id 对——上游视角等同官方新会话。

### TLS / HTTP 栈指纹（已核实到代码行）

路径：`default_client::create_client()` → `HttpClientBuilder`（默认
`TlsBackend::TransportDefault`，http-client/src/client_builder.rs:310）→
`build_with_transport_default_proxy_and_custom_ca_fallback` → 无自定义 CA 时直接
`reqwest::ClientBuilder::build()`（默认 TLS 后端）。**全程官方未 patch 代码。**

- reqwest 0.12.28（官方锁文件钉版）+ default-tls → native-tls：
  **Windows 上 = Schannel（系统 TLS 栈），Linux 上 = OpenSSL**——与官方同平台
  二进制同一后端。锁文件含 native-tls 0.2.14 / schannel 0.1.28 / hyper-tls /
  tokio-native-tls 佐证 default-tls 链完整在编。
- rustls（aws-lc-rs provider）仅在两条官方自有路径启用：配置自定义 CA
  （CODEX_CA_CERTIFICATE / SSL_CERT_FILE，custom_ca.rs:307）或 route-aware 池的
  TLS 回退重试（route_aware_client_pool.rs:799；我们的 ReqwestDefault 策略不经此路）。
- HTTP/2 指纹（SETTINGS/窗口/伪头序）由 hyper/reqwest 版本决定，官方 Cargo.lock
  复制钉版 → 与官方一致；连接池复用行为同为进程级单 client。
- ccodex 对 reqwest 唯一新增 feature 是 `socks`（代理解析能力），不影响 TLS。
- ALPN / 证书校验（平台原生根）均为 reqwest 默认，与官方一致。
- **平台形态自洽（`identity.ua_platform`）**：官方客户端 UA 的 OS/终端段是动态读本机的，
  TLS 栈也随平台（Windows=Schannel，Linux=OpenSSL）。ccodex 两种模式：
  - `native`（Linux 部署必选）：不覆盖任何 UA 段，官方动态探测原样生效——
    UA、TLS 栈与"官方 codex 跑在这台 Linux 机器上"逐字节一致（headless 时终端段
    为官方 fallback `unknown`，亦与官方一致）。
  - `windows`（默认）：固定 Windows 桌面指纹，仅在 Windows 主机运行时与 Schannel
    自洽；宿主机 OS build 若非 26200，用 `CCODEX_UA_OS_VERSION`/`ua_os_version`
    对齐真实 build 号。

## 三、刻意的偏差（全部评估为低风险，理由附注）

| 偏差 | 官方行为 | 理由 |
|---|---|---|
| 上行恒为 HTTP SSE，不用 WS | prefer_websockets 模型默认优先 WS | HTTP 是官方一等回退路径（WS 探测失败/被禁用即走此路），请求构造两条路径共享同一代码；WS 上行会把响应侧变成 ResponseEvent 再序列化（官方该类型只实现 Debug），漂移风险远大于传输层差异。若官方日后废弃 SSE 路径需改用官方 ResponsesWebsocketClient |
| 账号池轮换（429/5xx 换号 + 冷却） | 官方单账号 4 次指数退避重试 | 中转的核心价值；每个独立请求仍是官方形状；换号即全新会话/turn id（账号间零关联）；401 刷新+原地重试一次（UnauthorizedRecovery）按官方保留 |
| turn metadata 中无 workspaces | 官方 cwd 是 git 仓库时带 remote/commit | 与官方"非 git 目录"行为完全一致（non_empty_workspaces 过滤后省略）；中继无工作区，伪造 git 信息反而引入不一致 |
| 无 turn_trigger / root_turn_id / tool_namespaces_info | 默认配置下官方同样省略 | TUI 用户 turn 的 turn_trigger=None；root_turn_id 仅 steering/子代理流设置；tool_namespaces_info 需 opt-in 配置 |
| 多轮对话每请求新 turn_id、无 turn-state | 官方同 turn 内一致，跨 turn 新 id | 中继无状态 = 官方"每 turn 新 ModelClientSession"行为；turn-state 只在 WS 连接复用时重发（我们不复用上行 WS） |
| 中继重启后同一会话获得新 session/thread id | 官方进程内稳定 | id 映射在内存中（TTL 24h）；上游看到的是一次全新会话，属正常官方场景 |
| 匿名请求（无任何会话标识）每次全新会话 | — | 同官方单次 exec 场景 |
| sandbox/sandbox_mode 固定 "none"/"read-only" | 随用户配置变化 | 取 Windows 默认配置值；上游无法区分配置来源 |
| 自有错误响应（400/401/429）形状自拟 | — | 仅下行可见，上游不可见 |
| ~~Linux 构建 TLS=OpenSSL 而 UA 声称 Windows~~ | 官方 Windows 客户端 TLS=Schannel | **已消除**：`ua_platform = "native"` 时 UA 走官方动态探测，与 OpenSSL 栈自洽（官方 Linux 形态）。仅当在 Linux 上强行 `windows` 模式才存在该组合 |
| Linux 构建的 OpenSSL 次版本随构建机 | 官方 release 用其 CI 的 OpenSSL | cipher/扩展集合可能有细微差别；同在 OpenSSL 家族内，属官方 Linux 用户群体正常离散度。可用与官方 CI 相近的发行版构建进一步收窄 |
| /models、/v1/models 走缓存直出（内嵌官方 models.json 兜底 + 后台 60s 拉取、1h 刷新） | 官方 ModelsClient 请求路径 5s 超时（MODELS_REFRESH_TIMEOUT）实时透传 | 上游清单 ~260KB 且 CF 不压缩，高延迟代理链路上实测 20-28s，5s 必然 504；后台刷新请求形状与官方完全一致（`/models?client_version=<版本>` + 账号 auth/transport），仅时效性改为至多小时级。官方 CLI 自身也缓存模型清单。形状分工：`/models`、`/backend-api/codex/models` 下行字节即上游字节（codex 形状）；`/v1/models` 将同一份缓存转为 OpenAI `{"object":"list","data":[{"id"}]}` 形状并过滤 `visibility:"hide"`，服务 cc-switch 等 OpenAI 兼容工具（纯下行表示层转换，上游不可见） |

## 四、残余关注点（升级上游时复查）

1. models.json 行为字段变化 → `model_db.rs` 字段集需同步（sync 脚本 + 单测兜底）。
2. `CodexTurnMetadataPayload` 增删字段 → `build_turn_metadata` 需跟进（锚定补丁失效会阻断构建的是 vendor 侧，payload 是我们自有代码，需人工对照）。
3. WS 上行是否成为强制（届时接入官方 ResponsesWebsocketClient）。
4. `x-codex-turn-state` 语义变化（当前官方 HTTP 首轮恒无）。
