# ccodex

基于**官方 openai/codex 源码**构建的 Codex 中转网关。目标：上游看到的每一个字节都与官方 Codex CLI 完全一致，把风控风险降到最低。

## 原理

不复刻协议，直接**复用官方代码**。上行链路全部由官方 crate 驱动：

| 层 | 官方 crate | 负责 |
|---|---|---|
| 认证 | `codex-login` | auth.json 加载、access_token 主动刷新（JWT 临期 5min）、401 恢复 |
| 登录 | `codex-login` | 浏览器 OAuth 授权流（官方 `build_authorize_url`/`exchange_code_for_tokens`/`persist_tokens_async`）；备用设备码流 |
| 请求构造 | `codex-api` + 自有重建层 | `Provider`（URL + version 头）+ 按官方 `build_responses_request` 逻辑从头组装请求体/会话头（**不透传下游字段**） |
| 身份 | `codex-login::default_client` | `originator` 头 + 官方格式 User-Agent |
| 传输 | `codex-http-client` | reqwest + rustls、Cloudflare cookies、系统代理（含 socks5/socks5h 带认证） |
| 配额 | `codex-api::rate_limits`（补丁导出） | `x-codex-primary/secondary-*` 配额头解析 |
| 模型行为 | 官方 `models.json` / `prompt.md`（构建期内嵌） | 各模型 base instructions、Responses Lite 分支、默认 reasoning effort/summary、verbosity、node_repl 开关 |

自有代码只有几块：HTTP/WS 网关（axum）、**请求重建器**（`request_build.rs`：下行只提取
model/instructions/input/tools/reasoning/text.format 语义，上行请求体与全部客户端特征
按官方 0.153.4 构造逻辑与官方 models.json 行为参数重新生成）、账号池调度
（粘性会话/轮询/冷却换号）、SSE 旁路记账（逐字节透传，不改写）、管理面板。

为什么"重建"而不是"清洗透传"：清洗白名单只能保证字段集合不差，但 lite 模型的
instructions/tools 下沉、client_metadata 六键、turn metadata、v7 会话 id 等行为
必须主动构造才与官方一致。完整审计见 [docs/AUDIT.md](docs/AUDIT.md)。

## 目录结构

```
ccodex/
├── crates/ccodex/        # 中转器本体（Rust）
├── web/                  # 管理面板（Vite + React + shadcn 风格组件，构建产物内嵌进二进制）
├── patches/patches.json  # 锚定补丁清单（见下）
├── docs/AUDIT.md         # 行为一致性审计（基线 0.153.4）
├── tools/sync_upstream.py# 上游同步 + 补丁 + 验证脚本
├── tools/build_linux.sh  # Linux 构建（装依赖 + 同步 + 前端 + 编译）
├── .github/workflows/    # release.yml：Linux/Windows 双平台 CI 构建
├── upstream.lock.json    # 锁定的官方 commit
├── Cargo.lock            # 由同步脚本从官方复制（依赖钉版）
└── vendor/codex/         # 官方源码（脚本维护，勿手改）
```

## 快速开始

```sh
# 1. 同步官方源码并打补丁（需要代理时先 set HTTPS_PROXY=http://127.0.0.1:7890）
python tools/sync_upstream.py --check

# 2. 前端（可选，不构建则用占位页；中继功能不受影响）
cd web && npm install && npm run build && cd ..

# 3. 配置
cp config.example.toml config.toml   # 改 upstream_proxy；密钥全部在面板管理

# 4. 编译
cargo build --release

# 5. 添加账号（二选一）
./target/release/ccodex.exe login --name acc1                 # 浏览器 OAuth 登录（推荐，与官方 codex login 同流程）
./target/release/ccodex.exe import --name acc2                # 或导入本机 ~/.codex/auth.json

# 6. 启动
./target/release/ccodex.exe serve --config config.toml
```

OAuth 登录流程（无头服务器适配）：命令行/面板给出官方授权链接 → 浏览器完成登录后会跳转到一个
打不开的 `http://localhost:1455/auth/callback?...` 页面 → 把地址栏完整 URL 粘贴回来即可完成。
token 交换、API key 获取与凭证落盘全部走官方代码；备用设备码登录仍在（比 OAuth 更容易触发上游风控）。

启动后打开 `http://127.0.0.1:8317/` 进管理面板。**首次使用必须先设置面板登录密钥**
（强制设置页；SHA-256 哈希存于 admin.json，0600）。登录密钥与访问 /v1 接口的 sk 密钥
相互独立、不可混用（sk 泄露无法管理面板，登录密钥也无法调用模型）；忘记登录密钥时
在服务器上删除 admin.json 即可重新设置。
面板里也可以直接发起 OAuth / 设备码登录添加账号、查看配额与冷却状态、热重载账号池。

客户端接入（Codex CLI 的 `~/.codex/config.toml`）：

```toml
model_provider = "ccodex"
[model_providers.ccodex]
name = "ccodex"
base_url = "http://127.0.0.1:8317"
wire_api = "responses"
env_key = "OPENAI_API_KEY"   # 值填面板「密钥」页生成的 sk- 访问密钥（不是登录密钥）
```

## 端点

| 端点 | 说明 |
|---|---|
| `POST /v1/responses`、`/responses`、`/backend-api/codex/responses` | Responses 中继（SSE 透传） |
| `GET /v1/responses`、`/backend-api/codex/responses`（WS upgrade） | 官方 WS 协议：收 `{"type":"response.create",…}` 帧，事件帧逐字节回发；连接可复用 |
| `GET /models`、`/backend-api/codex/models` | 模型清单，codex 形状 `{"models":[…]}`，供 Codex CLI（缓存直出：内嵌官方 models.json 兜底 + 后台 60s 超时拉取/1h 刷新；官方 5s 交互超时在高延迟链路上不可取，见 [docs/AUDIT.md](docs/AUDIT.md) 偏差登记） |
| `GET /v1/models` | 同一份缓存转为 OpenAI 形状 `{"object":"list","data":[{"id",…}]}`（`visibility:"hide"` 条目过滤），供 cc-switch 等 OpenAI 兼容工具 |
| `GET /health` | 健康检查（账号数/上游 commit/身份版本） |
| `GET /` | 管理面板（内嵌静态资源，SPA 回退） |
| `GET /admin/api/auth-status` | 面板登录密钥状态（开放端点：`setup_required`） |
| `POST /admin/api/admin-key` `{current?, new}` | 设置/修改面板登录密钥（未设置时开放=首次强制设置；已设置需带当前密钥；不得与任一 sk 相同） |
| `GET /admin/api/overview` | 概览 |
| `GET /admin/api/accounts` | 账号列表（状态/冷却/email/套餐/上游配额快照/出口代理绑定/周期用量） |
| `POST /admin/api/accounts/reload` | 热重载账号池 |
| `POST /admin/api/accounts/oauth-login` `{name}` | 发起浏览器 OAuth 登录 → `{session_id, authorize_url}`（推荐） |
| `POST /admin/api/accounts/oauth-login/{id}/complete` `{redirect_url}` | 粘贴回跳的 localhost:1455 URL 完成登录（成功后自动热重载） |
| `POST /admin/api/accounts/device-login` `{name}` | 发起设备码登录（备用，更易触发风控）→ `{session_id, verification_url, user_code}` |
| `GET /admin/api/accounts/device-login/{id}` | 轮询登录状态 `pending/done/error`（done 后自动热重载） |
| `GET /admin/api/proxies`、`POST` 同名 `{name,url}` | 代理池列表 / 添加 |
| `DELETE /admin/api/proxies/{name}` | 删除代理（绑定账号回落默认出口并重载） |
| `POST /admin/api/proxies/test` `{name\|null}` | 经指定代理（null=默认出口，`"direct"`=直连）访问 IP echo 测出口与延迟 |
| `PUT /admin/api/accounts/{name}/proxy` `{proxy}` | 绑定账号出口（`"direct"` 或代理名，null 恢复默认；立即重建客户端） |
| `POST /admin/api/accounts/{name}/quota-refresh` | 主动查询上游配额/套餐（官方 `wham/usage`，走账号绑定出口并回填快照） |
| `GET /admin/api/keys`、`POST` 同名 `{name}` | sk 密钥列表（完整可见=找回途径；带用量统计）/ 生成密钥（`sk-`+96 hex；不得与登录密钥相同） |
| `DELETE /admin/api/keys/{name}` | 删除管理态密钥（面板用独立登录密钥，删 sk 不会锁死面板） |
| `GET /admin/api/pricing`、`PUT` 同名 | 模型定价表（官方定价页默认值 + 自定义覆盖，`"*"` 为未知模型兜底价） |
| `DELETE /admin/api/pricing/{model}` | 删除定价覆盖，恢复默认 |
| `GET /admin/api/upstream-version` | 官方最新版本（npm registry，4h TTL 缓存，访问时按需刷新） |
| `POST /admin/api/upstream-version/refresh` | 强制刷新最新版本 |

> 认证分两域：`/admin/api/*` 只认面板登录密钥（admin.json，哈希存储）；业务端点
> （/v1/*、/models 等）只认 sk 访问密钥（管理态 keys.json；旧配置 api_keys 启动时自动迁移并入），
> 两域互不通用。用量与成本按密钥指纹/账号归因，持久化在 usage.json（0600）；
> 成本 = 官方 API 定价换算的等效花费（订阅账号实际不按 token 计费）。

## 代理

`upstream_proxy` 支持 `http://` / `https://` / `socks5://` / `socks5h://`，可内嵌认证
（如 `socks5h://user:pass@127.0.0.1:1080`）。实现路径与官方客户端一致：配置写入
`HTTPS_PROXY`/`ALL_PROXY` 环境变量，由官方 reqwest 客户端的系统代理逻辑读取
（socks 能力经 feature unification 合入官方客户端，行为不变）。

**代理池**（管理面板「代理」页 / `/admin/api/proxies*`）：命名代理存放在
`accounts_dir/../proxies.json`，账号页可为每个账号选择出口（默认 / 池内代理 / `direct` 直连）。
绑定账号的数据面流量（Responses、Models）从对应出口出去：构建该账号的官方客户端时把代理
注入 reqwest 读取的环境变量（与官方同一条代理路径，指纹不变）。注意：token 刷新客户端由
官方 AuthManager 在刷新时重建，始终跟随环境默认（`upstream_proxy`），不随账号绑定。
改动绑定/删除代理会立即重建相关账号的客户端（热重载连接池）。

## 升级上游（核心工作流）

```sh
# 1. 改 upstream.lock.json 里的 commit 为新版本
# 2. 同步 + 补丁 + 验证 + 编译检查
python tools/sync_upstream.py --check
```

补丁系统是**锚定**的：每条补丁记录必须在官方源码中出现的特征串（anchor）和出现次数。
官方代码漂移导致 anchor 失配时脚本会**报错中止**，而不是静默产出行为不一致的构建。
补丁失效时的处理：打开报错文件，看官方改成了什么，更新 `patches/patches.json` 的 anchor。

当前补丁：

| 补丁 | 作用 |
|---|---|
| `runtime-version-override` | `get_codex_user_agent()` 的版本号改为运行时读 `CCODEX_CODEX_VERSION`（git 源码恒为 0.0.0，一眼假） |
| `export-rate-limits-module` | `codex-api` 的 `rate_limits` 模块 `pub(crate)` → `pub`（复用官方配额头解析） |
| `ua-fingerprint-env-override` | UA 的 OS/架构/终端段改为可读 `CCODEX_UA_*` 环境变量覆盖（`native` 模式不设置即官方动态探测） |

同步脚本同时把官方 `Cargo.lock` 复制为本 workspace 锁文件——
官方依赖必须钉在官方测试过的版本（semver 自动升级实测会破坏 rama 系列）。
上报版本号缺省跟随编译时烘焙值（sync 时从 npm registry 解析当前发布版本），
不自动跟随更新，升级上游后重新同步编译即可。

## 行为一致性清单（冒烟测试已验证）

总原则：**不透传**。下游客户端的字段/头一律不进入上行请求；上行请求按官方
`build_responses_request` 从头组装，行为参数取自官方 models.json（lite 分支、默认
reasoning/verbosity 等）。完整审计（含刻意偏差及理由）见 [docs/AUDIT.md](docs/AUDIT.md)。

请求上游 `chatgpt.com/backend-api/codex/responses` 时：

- `Authorization: Bearer <账号 access_token>` + `ChatGPT-Account-ID`（FedRAMP 账号自动加 `X-OpenAI-Fedramp`）
- `originator: codex_cli_rs` + `User-Agent: codex_cli_rs/<version> (<OS>; <arch>) <terminal>`（官方代码生成；`identity.ua_platform = "native"` 时与官方一样动态探测本机，Linux 部署必选以与 OpenSSL 栈自洽；默认 `windows` 为固定 Windows 指纹）
- `version` 头与 UA 同版本
- `session-id` / `thread-id` / `x-client-request-id`：**v7 UUID**（与官方 `SessionId/ThreadId::new` 同版本），同下游会话稳定复用、多用户互不关联
- `x-codex-window-id: {thread_id}:0` + `x-codex-turn-metadata` 头（官方 CodexTurnMetadataPayload 全字段）
- 默认**不发** `x-codex-beta-features` / `x-codex-turn-state` / `conversation_id` / traceparent 等（对齐官方默认配置首轮行为）
- `Accept: text/event-stream`，请求体 `Content-Encoding: zstd`（官方默认开启压缩）
- 请求体字段集合与顺序 = 官方 `ResponsesApiRequest` 结构体序；`store:false`、`stream:true`、
  `include:["reasoning.encrypted_content"]`、`tool_choice:"auto"`、`prompt_cache_key=session_id`
- **Responses Lite 模型**（gpt-6-astra、gpt-5.6-sol/terra/luna 等当前主力）：
  instructions/tools 下沉为 input 前缀项（`additional_tools` + developer message，
  id 为官方 v5 派生），`parallel_tool_calls=false`、`reasoning.context="all_turns"`、
  发送 `x-openai-internal-codex-responses-lite: true` 头；**非 lite 模型**则顶层
  instructions + tools、`parallel_tool_calls=true`
- reasoning effort/summary、text verbosity 按"用户显式值 → 模型名后缀 → 官方 models.json
  默认值"解析（含 ultra→multi_agent/max 映射、json_schema output format 形状）
- `client_metadata` 官方六键：installation_id（按账号目录持久化，v4）/ session_id /
  thread_id / window_id / turn_id（v7，每请求新生成）/ turn-metadata
- 无前缀 item id 按官方规则剥离；lite 模型剥离 image detail；
  `temperature`/`max_output_tokens`/`previous_response_id`/`metadata` 等官方不发的字段一律剥离
- GET /models 刷新请求形状对齐官方（只发 `client_version=<版本>` query），但超时放宽到 60s
  走后台刷新（官方 5s MODELS_REFRESH_TIMEOUT 在高延迟链路上不可取，见 AUDIT 偏差登记）
- token 临期自动刷新、401 先刷新原地重试一次（对齐官方 `UnauthorizedRecovery`）
- WS 下行：上行始终走官方 HTTP SSE 路径（官方客户端在 WS 不可用时的同款回退行为，
  请求构造共享同一代码），事件内容原样转发，不做解析/重序列化

## Linux 编译

官方链路含 C 依赖（zstd/ring/aws-lc/openssl），需要在 Linux 机器上本地编译
（或 CI）：`bash tools/build_linux.sh`（装 `build-essential pkg-config libssl-dev` 后
同步、构建前端、编译）。`.github/workflows/release.yml` 提供 Linux/Windows 双平台构建参考。

**Linux 部署时把 `identity.ua_platform` 设为 `"native"`**：UA 走官方动态探测（真实 Linux
形态），与 native-tls→OpenSSL 的 TLS 栈自洽——出口流量的 TLS 指纹、HTTP/2、UA 与
"官方 codex 跑在这台 Linux 机器上"逐字节一致。默认 `windows` 模式仅在 Windows 主机
运行时才是自洽组合（Schannel + Windows UA）。

## 注意事项

- 中转共享的是 ChatGPT 订阅额度，违反 OpenAI ToS，账号有封禁风险
- 会话粘性：同会话请求尽量落同一账号（保 prompt cache），`sticky_ttl_secs` 控制 TTL
- 429/5xx/网络错误按账号冷却 + 指数退避换号；4xx 请求问题直接镜像给下游
- `identity.version` 一般留空用烘焙值；如上游发布新版客户端而镜像未升级，可临时手工指定
- 开发自检：`CCODEX_UPSTREAM_BASE_URL_OVERRIDE=http://127.0.0.1:9123` 可指向本地 mock；
  `testdata/mock_upstream.py` + `testdata/verify_smoke.py`（HTTP）+ `testdata/verify_ws.py`（WS）是配套冒烟工具
