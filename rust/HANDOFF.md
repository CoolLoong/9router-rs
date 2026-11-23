# 当前交接

基线和范围见 [MAP](MAP.md) 与 [决策](DECISIONS.md)。

固定上游的网关、管理台、CLI 和 Provider 已有 Rust 实现。175 个 API 路径、290 个 HTTP 方法和 145 个 Provider ID 已核对。Rust 启动及请求处理无 Node 回退。MiMo 独立网页登录和 Grok Web 推理已移植。Grok Web 使用常规 HTTPS，遇到上游挑战时返回错误。

最终 Linux 默认配置和无默认特性配置均通过 `cargo check --locked --offline`。本次收尾跳过逐项回放、Trae 和 CodeBuddy 国际版专项验证、macOS 和 Windows 平台验证以及真实厂商联调。

## 当前结果

- 本轮恢复迁移后，Linux 独立版通过已有 859/859 条 HTTP 回放，零回退。Trae 和 CodeBuddy 国际版已接入原生授权和推理，按验收范围跳过专项测试。macOS 和 Windows 本轮不做平台验证。
- MiniMax Code 两区接入原生 Messages 请求、设备令牌续期、桌面租约和额度。离线验证了 Unix socket 帧、幂等添加、401 后续租、Messages 转换和额度签名。设备授权的原版对照仍通过。
- Copilot Embeddings 原生回放 18/18，覆盖使用现有 Copilot 会话的向量请求、401 后交换并写回会话，以及 CONNECT、SOCKS5h 和显式绕过。Relay 暂不支持。
- Edge/Bing 和 Google Translate 语音原生回放 30/30，模拟上游对照覆盖 Bing 令牌重取、Google 批处理解码和 MP3 输出。
- Grok CLI 的 Anthropic 思考配置、历史块和内联图像由 Rust 转换，3/3 个出站请求与原版相同。隔离网关验证了非流式 JSON 和思考 SSE。URL 形式图像和非流式思考摘要沿用原版处理。
- NanoBanana 通过本地模拟验证提交、轮询和错误脱敏。运行时由操作者提供 `NINEROUTER_NANOBANANA_CALLBACK_URL`，需由供应商访问的授权 HTTPS 地址。
- CodeBuddy 中国版验证了原版请求改写、强制流式上游、401 续期、SQLite 写回和限流时间。原有设备授权场景 3/3。
- 已迁移网页账号会话和连接验证。模拟浏览器协议验证了 Profile 占用、指定域 Cookie 过滤、SQLite 写入、取消和响应脱敏。
- MITM 管理台、状态、路由地址及模型映射接入 Rust。隔离验证覆盖本地权限、模型映射保存和缓存同步，已有回放 5/5。
- MITM 根证书生成、校验和信任接口已移植，DNS 开关保留手工 hosts 记录并在刷新或提交失败时回滚。原生信任和 DNS 写入要求进程已有管理员权限，实测使用临时证书和 hosts。
- MITM 启停、根证书生成、回环 TLS 监听及退出清理已接入原生网关。隔离验证覆盖 HTTP/1.1 与 HTTP/2、Copilot 转发、Kiro EventStream 帧和 CRC、Antigravity SSE、Cursor 的上游 501、TLS 主机约束及固定域名透传。透传校验上游证书，并使用请求的原始主机。
- MITM 已验证监听退出、自身进程崩溃和正常重启后的恢复。恢复复用原证书，并核对原先选定的活跃 Key 和路由地址。密钥失效或地址改变时清理本实例的残留映射，显式停止取消恢复。隔离使用临时 hosts 和 SQLite。
- Kiro 旧版刷新令牌导入与已保存的兼容账户通过 Linux 直连、CONNECT 和 SOCKS5h 对照，涵盖模型发现、连接测试和推理。受保护出口仍要求可识别的授权信息。
- Qoder 两区动态目录、PAT 兑换、COSY 签名、连接 Test/Validate 和推理已接入。隔离验证覆盖图片上传、队列轮询、Anthropic SSE、额度错误及目录缓存。中国区图片留在中国区网关。
- Copilot 目录的管理台和公开入口支持 CONNECT、SOCKS5h 和显式绕过。隔离网关验证了 403 后换取会话并写回、按出口隔离缓存和隧道失败拒绝降级。专项驱动已改用真实网关，原版对照 21/21。原 force 用例验证重启后的冷缓存，因为公开 API 没有强制刷新入口。
- Provider 批测与固定上游 12/12 一致，Validate 49/49 一致，另验证 12 个代理场景。24 个 Provider 的已保存 API Key 测试通过 72/72 个回环用例。
- Perplexity 已保存会话 Test 通过九组隔离验证。Bedrock 静态 AWS Profile 的 Test 和 Validate 已通过本地签名请求及 SOCKS5h，无配置的 Profile 从原生端点返回错误。
- Bedrock 命名 Profile 的 STS 角色链、SSO Portal 和 OIDC 续期按各端点选择受保护出口。隔离验证覆盖 CONNECT、SOCKS5h、显式绕过、按路由隔离缓存及失败拒绝降级，原版回放 2/2。
- Codex OAuth 图片生成和编辑已接入。请求体与固定上游对照 4/4，覆盖专用图片接口和 Responses 工具。隔离验证覆盖二进制、SSE、用量写回、CONNECT/SOCKS5h、同路径续期和提交结果不明时停止重试。
- Windsurf 原生授权、聊天和 Translator 已接入。隔离验证覆盖回环回调、JWT 换取密钥、资料写回和拒绝未知上游。Protobuf 与固定上游一致，验证了直接 JSON、CONNECT 和 SOCKS5h、拆分 gRPC-Web 帧、用量、错误脱敏和代理失败。聊天使用保存的 Codeium 或 Windsurf 自助服务地址。保留固定上游的目录隐藏状态，使用完整 `windsurf/<model>` ID。
- ChatGPT Web 的原生文本路径接入了已有增量解码器。临时会话可使用已选 Luna Free，也可从可见菜单选择 GPT‑5.5 Instant 和 GPT‑5.6 Sol Instant。页面完成后返回 SSE。Chat Completions 和 Translator 支持内联 Base64 文件。Chat、Messages、Responses 和 Translator 路径已验证内联 PNG、JPEG、GIF、WebP，单张限 20 MiB，附件合计限 50 MiB、最多 10 个。隔离合成 CDP 56/56，覆盖字节、格式和尺寸、临时文件清理及路由守卫。仅声明 MIME 的文件选择器和真实网页仍有限制。
- ChatGPT Web 补齐 Messages 的内联 Base64 PDF、Responses 的内联 `file_data` 和对应 Translator 转换，复用现有附件装载与限额。文件 ID 和远程文档地址未接入此入口。此批仅编译检查。
- ChatGPT Web 接入客户端函数工具，输出 JSON、SSE、Messages、Responses 和 Translator 的结构化调用。合成 CDP 79/79，覆盖并行调用、指定或禁用工具、声明名单和每请求绑定值。显式工具标签支持单引号、裸键、Python 字面值和尾逗号。请求绑定值不进入调用参数、响应或日志。函数执行由调用方负责。
- ChatGPT Web 增加 ASCII 和全角 DSML 工具标签。工具名称须在当次请求声明中，DSML `_nonce` 必须与当次请求匹配；随后移除绑定值并输出调用参数。裸对象及校验不通过的内容继续保留为文本。此批仅编译检查。
- ChatGPT Web 的普通附件选择器支持扩展名、精确 MIME 和标准 MIME 大类，附件经原有内容检查后再交给浏览器，提交前复核实际文件。固定上游的附件协议使用 Base64 和 data URL。此批仅编译检查。
- Gemini Web 原生接入已保存 Cookie 的聊天、增量 SSE、Translator 和图片生成。沿用固定上游的普通网页输入、模型状态确认及回复 DOM；图片从已渲染的回复中读取 PNG。浏览器通道接受直连或显式代理绕过。此批仅编译检查。
- DeepSeek Web 原生接入已保存 Bearer 凭据的文本、JSON/SSE、已上传文件 ID 和持久会话。按安装目录、账号、调用方与出口隔离会话；可信调用方启用 `persistSession` 时，在成功完成后复用同一会话。复用请求收到明确 404/410 时新建一次。普通 HTTP 客户端仍待真实厂商验证。此批仅编译检查。
- DeepSeek Web 支持声明的客户端函数和工具历史，返回 JSON 或带索引的 SSE 调用。保留名称和当次请求绑定，入参使用离线 JSON Schema Draft 2020‑12 校验，按固定上游配置关闭格式校验。支持本地引用，校验过程不请求外部引用，Schema 无法编译时该工具不产生调用。工具由调用方执行。此批仅编译检查。
- ChatGPT Web 的可见选择器补齐 GPT‑5.6 Sol 和 GPT‑5.5 的 Thinking、Pro，以及 Luna Free Thinking，沿用固定上游的推理参数优先级和 Instant/Thinking 映射。找不到对应选项时返回错误。此批仅编译检查。
- Perplexity Web 已接入直接 JSON、SSE 和 Translator。请求体与固定上游对照通过，隔离验证覆盖保存的 Cookie 和 Bearer、流内错误脱敏，以及按调用 Key、账号凭据和代理路径隔离续接 ID。请求使用 9Router 客户端标识，真实服务接收情况待验证。
- Cursor 旧版 Connect 的请求和响应对照分别通过 6/6、16/16。网关接管符合原版旧通道条件的空消息请求，Translator 可发送兼容的已转换请求。常规 AgentService 聊天仍走兼容入口。
- Devin CLI 文本 ACP 已接入 Chat、Messages 和 Responses。隔离验证只启动模拟进程，覆盖流式输出、拒绝权限与文件请求、临时目录和断开清理。入口保持隐藏，要求从回环地址使用活跃 Key 并由非管理员网关运行。客户端工具桥接待迁移。
- 上游内置 Antigravity 普通推理已接入五种固定模型映射、JSON/SSE 和受保护 OAuth 续期。隔离验证覆盖项目约束、CONNECT/SOCKS5h、限流响应保留 `Retry-After`、403/429 终止同次调用及拒绝代理降级。请求使用 9Router 客户端标识，实际供应商未实测。
- 同一 Antigravity 传输已接入图像生成。隔离验证覆盖参考图数量、宽高比选择、负面提示、包装与非包装响应、二进制输出及 CONNECT 下 429 保留 `Retry-After`。
- UC Web 图片、文生视频和图生视频已接入。图像与文生视频请求体与固定上游对照一致，隔离验证覆盖 Clerk 并发轮换及写回、签名上传、轮询下载、结果封装和凭据隔离。受保护出口的用户输入图使用内嵌数据，直接出口可读取通过地址校验的 HTTPS 图。视频在创建响应中直接返回完成结果。
- Cursor AgentService 已接入普通聊天及 Translator。隔离 HTTP/2 验证通过 9/9，覆盖固定上游请求帧、双工上下文、分段压缩输出、声明工具转发、未知工具错误、同路线模型目录、空响应重连及凭据脱敏。目录和旧版 Connect 对照仍通过。
- Devin 客户端函数桥已接入原生 ACP，使用独立临时目录与固定 stdio 子命令。合成验证覆盖工具声明、历史结果、客户端工具事件和权限拒绝。真实 Devin summarizer 能否发现 MCP 工具仍待验证。
- 上游内置 Antigravity 已接入原生 OAuth 授权和账号激活。Linux 隔离验证覆盖一次性状态和 verifier、保存身份与项目后才发出激活请求、重新授权更新同账号、CONNECT 出口和失败不降级。
- 上游内置 Antigravity 的 `/v1/search` 已接入，复用原生项目鉴权、OAuth 续期与受保护出口。引用顺序、去重和 UTF-16 提取与固定上游一致；隔离验证覆盖直连、CONNECT、SOCKS5h、401 后续期和失败拒绝降级。
- Gemini API Key 搜索已接入 `/v1/search` 及两个兼容路径。请求和引用结果与固定上游对照通过，覆盖直连、CONNECT、SOCKS5h、TLS 失败和代理失败。引用过滤到 HTTP(S)。
- Linkup 搜索已接入 `/v1/search` 及两个兼容路径。与固定上游对照通过，覆盖深度优先级、域过滤、UTC 日期范围、结果上限和计费字段。隔离验证覆盖直连、CONNECT、SOCKS5h、TLS 及代理失败。
- TinyFish 搜索已接入 `/v1/search` 及两个兼容路径。与固定上游对照验证搜索类型、分页边界、结果排序、计费和区域及域名过滤。代理验证覆盖 CONNECT、SOCKS5h、TLS 和拒绝降级。
- Xquik 搜索已接入 `/v1/search`。固定上游对照覆盖游标分页、帖子链接编码、返回条数计费和错误脱敏，隔离通过直连、CONNECT、SOCKS5h、重定向和代理失败处理。
- Z.ai GLM Coding 搜索支持手动 API Key 和 CLI 登录生成的 Coding Plan Key。JSON-RPC 请求与固定上游对照通过，覆盖多种结果封装、错误脱敏、直连、CONNECT、SOCKS5h 和代理失败。
- Ollama Cloud 搜索优先使用搜索专用账号；没有可用专用账号时才选聊天账号的 Key。专用账号限流沿用专用账号的冷却，复用聊天账号时只更新该账号的搜索冷却。隔离覆盖上游请求、封装结果、代理认证、TLS 及失败处理。
- Perplexity Sonar 已接入 API Key 搜索，返回回答、HTTP(S) 引用和 Token 用量，复用原生受保护搜索出口。此批仅编译检查。
- Perplexity Agent 原生搜索复用受保护出口，支持 `pplx-agent` 和 `pplx-responses`，从 Responses 结果提取回答、引用和 Token 用量。此批仅编译检查。
- Muse Web 的普通文本聊天和 Translator 已接入原生网关。合成 CDP 进程验证了原版单轮与多轮提示词、Cookie 轮换、JSON/SSE、额度错误、停止于审批和临时目录清理。与固定上游一致，`tools` 和 `functions` 分别返回 400，历史工具结果保留为提示词文本。配置隧道时需要已有明确绕过才能进入浏览器通道。
- Muse Web 图像与视频已接入，合成验证 7/7。覆盖多图、文生视频、图生视频、审批中止、输入图和附件来源校验。原生下载接受受验证的同源媒体，隔离试验未启动真实浏览器。
- Bedrock 的命名 AWS Profile 支持 Web Identity 凭据文件与后续签名角色链。与固定上游 SDK 的 STS 表单字段对照通过，隔离验证覆盖投射令牌的符号链接、直连、CONNECT、SOCKS5h、缓存隔离和隧道失败拒绝降级。现有 Profile 和 SSO 回归通过。`credential_source=Environment` 支持环境变量短期凭据，复用当前签名角色链及代理策略。隔离回放验证了 STS 签名、推理和隧道失败处理。
- Bedrock 支持本机 AWS Profile 声明的 `credential_process`，由程序输出版本 1 的凭据，可独立使用或作为签名角色链的起点。固定上游对照 7/7，管理接口的未授权请求和无效程序输出验证通过。Linux 隔离验证确认超时会结束程序及其子进程，错误响应省略命令和进程输出。
- Bedrock 的 `EcsContainer` 和 `Ec2InstanceMetadata` 支持 ECS 相对 URI、EKS 容器地址、令牌文件与 IMDSv2。12 项隔离场景通过，其中六条成功路径与固定 AWS SDK 请求顺序一致。IMDSv1 遵守禁用设置，只有令牌请求超时或返回 403、404、405 时才尝试。外部 HTTPS 凭据服务沿受保护代理发送，隔离 CONNECT 验证失败时不会转直连。
- Bedrock 的 AWS CLI `login_session` 可从本机缓存读取凭据，并沿固定 Sign-In 地址以 ES256 DPoP 续期。签名与请求字段和固定 SDK 对照通过。覆盖直连、CONNECT、SOCKS5h、两个进程共享续期、外部缓存优先、原子写回和代理失败。原有 IAM Identity Center 与 Web Identity 回归通过。
- Perplexity 未保存 Cookie 的 Validate 复用原版 ping 并丢弃响应正文，隔离验证覆盖直连和代理路由，现有 Validate 对照通过 61 项。
- Grok Web 的未保存 Cookie Validate 和已保存连接 Test 共用原生只读会话查询。仅发送 `sso`，获得认证状态才判有效，挑战和限流显示无法确认。隔离验证覆盖直连、CONNECT、SOCKS5h、绕过配置、重定向和不落库。
- Grok Web 原生接入已保存会话的文本 JSON/SSE、Translator 和 Imagine 图片与视频。使用固定会话地址和第一方素材域名，普通 HTTPS 出口遇到上游挑战会返回错误。视频按上游文本生成协议接入，带图编辑明确返回不支持。默认和 `native-package` 的 Linux `cargo check` 已通过。
- Grok Web 文本支持请求声明的客户端工具、`none` 和指定工具，返回 JSON 或带索引的 SSE 调用。校验名称和当前请求绑定，再输出结构化调用；服务端原生工具卡片不转换为客户端操作。此批仅编译检查。
- Veo AI Free 的图片和视频已走原生 WordPress AJAX 会话。涵盖表单 nonce、图片结果、视频状态查询和受限素材下载，非第一方素材需要允许的直连或显式绕过。此批仅编译检查。
- Linux Tailscale 使用自执行引导替代 `unsafe pre_exec`。隔离验证确认私有锁继承、父进程死亡后子进程退出、重启恢复和无关进程隔离。
- Profile 已补 Provider 账号及组合的独立轮询开关和粘滞次数。隔离 HTTP 和脚本验证了持久化、关闭时映射到 `fill-first`/`fallback` 及次数保留；现有 SAML、OIDC 和备份流程也通过。
- Profile 的手动关停接入已鉴权的 `/api/version/shutdown`，需要在页面确认。隔离验证覆盖取消、重复提交、失败后重试和临时进程正常退出。
- Codex 账号管理补齐 Bulk Add，支持单对象、数组和 `accounts` 包装，逐条显示脱敏错误，成功后清空输入并刷新列表。隔离验证覆盖部分成功、全部失败和 SQLite 读回。
- MiMo 管理台已支持本机凭据重同步和官方 API Key 授权。加密授权使用固定厂商网址、独立回环回调、X25519 与 AES-256-GCM。隔离验证覆盖固定上游 URL、会话隔离、错误回调和重放、保存后的原生聊天。重同步复用原账号并保留会话字段。授权页面由用户开启，真实厂商未联调。
- MiMo 在直接回环访问时，可随本机导入或官方加密授权关联同 UID 的 Desktop 会话。只读查询指定账号域名的三个原有字段。隔离验证覆盖 UID 不符、重复项、数据库锁、符号链接和加密值，遇到这些情况只保留 API Key。转发请求不会附带本机会话。
- MiMo Desktop 独立网页登录已接入原生开始、状态和取消接口，以及管理台入口。登录页使用单独的 `127.0.0.2` 临时端口，厂商 Cookie 保存在本次会话内，成功后由后端保存到所选地区。现有加密 API Key 和导入入口也可使用。默认和 `native-package` 的 Linux `cargo check` 已通过。
- MiMo Desktop 已保存会话接入原生三种 v2.6 模型的聊天、周额度和连接测试。两阶段交换保留 64 位 nonce 和 SHA-1 签名，第二阶段省略 Cookie，按账号、地区与出口隔离缓存。隔离验证覆盖五区、CONNECT、SOCKS5h、并发 401 只续期一次、配额日期和云 API Key 共存。CN 也遵守网关出口设置。连接测试遇到代理或上游故障会返回未通过，同时保留现有账号状态。
- MiMo 加密授权生成的云 Key 可直接合成语音，已验证直连、CONNECT 和 SOCKS5h。混合账号下跳过只有 Desktop 会话的记录；只有会话时返回缺少云 Key。五例语音输出与固定上游对照一致。
- Headroom 保存代码感知压缩或 Kompress 后会重启正在运行的本机托管实例。隔离验证检查了新 PID 和命令行参数，失败时页面区分设置已保存与重启未生效。
- Claude 和 Codex 的 OAuth 连接可在 Provider 详情直接配置额度自动探测和每日定时探测，与额度页共用表单和设置接口。此批仅编译检查。
- 隧道页可复制运行中的 Cloudflare 和 Tailscale API 地址，保留反向代理路径并避免重复 `/v1`。模拟剪贴板验证了公开与备用地址、离线状态及复制失败时的文本回退。
- Usage 请求详情可一键清除 Provider、模型和日期筛选，并回到未筛选第一页。隔离 HTTP 验证了迟到的旧筛选结果不会覆盖新列表。
- Usage 请求详情新增每页 10/20/50 条、当前记录范围和直接跳页，切换每页条数回到第一页。此批仅编译检查。
- Translator 第 3 步可用已保存项目 ID 生成 Gemini CLI Cloud Code Assist 预览，令牌使用占位值。此批仅编译检查。
- Gemini CLI Provider 模型发现已原生接入 `fetchAvailableModels`，读取已保存的 OAuth 凭据和项目，归一化数组或模型映射。401/403 只在续期成功且出口一致时重试一次。此批仅编译检查。
- Codex Provider 模型发现使用已保存 OAuth 凭据请求固定 Codex 目录并生成适用的 `-review` 变体。401/403 时只在同一出口续期并重试一次。此批仅编译检查。
- Fish Audio 原生 TTS 走固定接口，支持默认模型与音色引用，复用网关代理和音频响应处理。此批仅编译检查。
- Cartesia 原生 TTS 接入既有模型与音色引用格式，使用固定 `tts/bytes`、Cartesia-Version 和原有 MP3 参数。此批仅编译检查。
- Claude OAuth 连接的模型目录走原生 Anthropic Models 请求，沿用已保存令牌、专用请求头及代理出口。此批仅编译检查。
- xAI 视频创建及查询接入已保存的 OAuth 凭据。401/403 时按固定 Token 地址和所选代理续期，视频出口一致才重发一次。此批仅编译检查。
- xAI 图片请求接入已保存 OAuth 凭据，临近过期时提前续期，与视频共用固定 Token 端点及代理选择。此批仅编译检查。
- 自定义转录模型可使用固定上游的 `nvidia-asr` 传输标记，复用原 Provider 的端点、认证和代理设置，发送 multipart `file`/`model` 并统一 `text` 与 `transcript` 返回字段。Gemini 原生 TTS 的受保护出口可使用已保存的 Bearer 凭据。此批仅编译检查。
- Kiro 旧版 IAM Identity Center 可分别保留授权区和推理区。隔离验证覆盖 `eu-west-2` 授权配合 `us-east-1` Profile，在直连、CONNECT 和 SOCKS5h 下完成模型查询、连接测试及推理。
- Kiro 旧版 Builder ID 在缺少 AWS 客户端凭据时使用固定的 Kiro 刷新端点。与固定上游对照通过，Linux 隔离验证覆盖直连、CONNECT、SOCKS5h、令牌轮换及 Profile 保留。客户端凭据不完整和授权端点被排除的请求保持失败。
- MITM 支持在系统允许绑定端口且无需修改 hosts 时，由非管理员启停本实例。隔离验证覆盖回环鉴权、实例锁、hosts 符号链接和残留映射拒绝。该模式只维持当前运行，证书信任和 DNS 写入仍要求管理员权限。

- 原版和 Rust 入口均通过 859/859 条 HTTP 回放，见 [回放报告](app-cases/replay-report.md)。Linux x86-64 和 QEMU 模拟的 Linux ARM64 独立构建均通过 859/859，零回退。带原版子进程的构建有三次 `PATCH /api/settings` 交给原版协调进程状态。
- [本轮原生预览包](native-package/dist/speech-native.UVYrjHBF) 包含 OpenRouter 与 Ollama Local TTS，以及媒体观测开关修复。两份 Linux 静态包解包后各通过 859/859，零回退，ARM64 运行于 QEMU。x86-64 包另过九项观测模式回放与 Ollama 直连对照。Windows 发行 PE 在隔离 Wine 中通过四组签名更新和恢复。
- [本轮原生预览包](native-package/dist/tunnel-native.JBypevvy) 包含旧版 Cloudflare 状态接管、Provider 分组批测、Vertex 预览和 Gemini CLI 额度代理。实际解包的两份 Linux 静态发行版均通过 859/859，零回退。Windows PE 在独立 Wine 中通过旧隧道接管及四组签名更新和恢复场景。
- [本轮原生预览包](native-package/dist/provider-native.SexrK5Ac) 包含兼容端点管理、Embeddings、Vertex/Partner Chat 与 Translator 的改动。解包的 Linux x86-64 和 ARM64 静态二进制分别通过 859/859 条回放，零回退，ARM64 使用 QEMU。x86-64 归档另外通过 9/9 条 Embeddings 专项回放和 14 项端点验证对照。Windows ZIP 在隔离 Wine 中通过 Headroom 生命周期及四组签名更新与恢复。
- [最新原生预览包](native-package/dist/headroom-native.kua6TrJp) 包含 Windows Headroom 进程管理和 SAML 供应商说明。解包的两份 Linux 静态二进制在空 `PATH` 下启动，分别通过 859/859 条回放，零回退。ARM64 由 QEMU 运行。Windows ZIP 校验 PE 摘要和 397 项依赖许可后，解包通过 Headroom 生命周期及四组签名更新和恢复用例，使用独立 Wine。
- [本轮 Linux x86-64](native-package/dist/integrated-native.3sGuZuHS/9router-0.5.119-native-preview-x86_64-unknown-linux-musl.tar.gz) 和 [Linux ARM64](native-package/dist/integrated-native.3sGuZuHS/9router-0.5.119-native-preview-aarch64-unknown-linux-musl.tar.gz) 归档均通过静态链接检查与空 `PATH` 启动检查。实际解包的两个发行版二进制均通过 859/859 条回放，零回退。ARM64 使用 QEMU。
- [本轮 Windows 归档](native-package/dist/integrated-native.3sGuZuHS/9router-0.5.119-native-preview-x86_64-pc-windows-gnu.zip) 校验 PE 摘要和依赖许可后，实际解包运行并通过 Wine 中的四组签名更新和恢复用例。升级目标沿用已验证的 `0.5.120` 测试版本。
- [管理台检查点预览包](native-package/dist/dashboard-native.qJOqiB0h) 包含媒体组合、Profile 的 SSO 和备份、Headroom 页面、额度查询与 System One 代理改动。两份 Linux 静态归档解包后通过 859/859，回退为零，ARM64 使用 QEMU。Windows 归档校验摘要和 397 项依赖许可后，在 Wine 中通过四组签名更新与恢复。该检查点先于 SAML 供应商说明卡片。
- Rust 初始化 11 张 SQLite 表，读取公开登录设置，并接管 API Key、Provider 连接、兼容节点、模型禁用、自定义模型、别名和组合的常规管理接口。节点变更与连接传播同事务，删除清理关联连接和自定义模型。原版进程缓存相关的更新、预设与在线验证有意保留回退。
- 已接入模型列表与单条查询 `/v1/models`、`/v1/models/{kind|id}`、`/v1/models/info`、`/v1` 和 `/api` 前缀路径。目录合并启用/禁用、组合、自定义模型、别名和能力规则，按受限 Key 过滤。兼容模型发现、标准连接检测及真实环回模型探测已验证。专属 Provider 的动态解析器和少数代理发现路径继续迁。
- 相关 rust-anything gate：Provider 连接脱敏 21/21，Provider 模型元数据 266/266，纯模型解析 406/406，Provider 能力和组合聚合 751/751；模型解析另有同库的 HTTP 回放 44/44。
- 从固定上游提交重新导出的 145 个管理台 Provider、126 个模型别名和 1,474 项模型记录与 Rust 嵌入数据一致。聊天传输表和默认请求头的导出也一致。
- 直连 OpenAI Compatible Chat 由 Rust 校验模型与权限，处理请求与流式响应，执行账号冷却、失败切换和粘滞轮转。同源 307 保留请求体与凭据。独立账号回放 15/15，账号规则 gate 299/299，请求整形 28/28。
- Anthropic Messages 和 OpenAI Responses 可用 Rust 同步或流式接到 Chat 上游，覆盖文本、推理、多块工具参数、缓存用量、断流，以及无用量尾帧的 Responses 结束。Claude SSE gate 105/105，Responses SSE 独立回放 20/20，跨协议输入 30/30。
- 配置为 Responses 的兼容节点已接入 Rust。Responses 可直接使用 JSON 与 SSE，并保留续接、include、store 字段。Chat/Messages 可聚合上游 SSE，也可逐帧转换。请求构造独立回放 26/26，Responses 上游 8/8 与额外边缘场景 6/6，反向流状态机 7/7。HTTP 200 携带损坏 JSON 已录制，Rust 按原版持久化冷却并返回 503。
- Anthropic 兼容 API Key 上游接入了 Messages 直通和 Chat/Responses 请求转换，覆盖 JSON、SSE 和工具增量。请求回放 31/31，响应转换 gate 114/114。
- Rust 在同一事务写入用量历史、SQLite 日汇总和生命周期计数。分别对 Chat 和 Responses 上游录制一次 JSON 与一次 SSE，累计用量、缓存和各组计费均匹配原版。
- 密码登录和设置回放 22/22，实际签发的 JWT 和 bcrypt 密码经原版与 Rust 双向读取验证。SQLite 原生启动迁移回放 5/5，包含旧 JSON 导入、重复 ID 回滚、旧表加列、备份及重启后的持久数据。
- Gemini API Key 聊天已覆盖生成 JSON/SSE、工具签名重放和 Gemini 文本接口。Embeddings 覆盖 API Key 兼容路径和 Gemini 单条、批量。TTS 已录制 OpenAI、自托管和 Gemini PCM→WAV，隔离检查确认对应字节、音频格式和响应头。
- CLI 参数及九种工具的 `show` 独立回放 44/44，原生 `xai video` 独立回放 19/19，含二进制下载、取消与中途截断。
- CLI `connect` 44/44，含九种工具写入与重置、符号链接、私密密码输入及终端恢复。Linux 托盘 6/6，隔离二进制验证 SIGHUP 后仍响应、SIGTERM 正常退出。
- 组合回放覆盖 JSON/SSE 空响应、多级组合、请求错误、受限 Key 和粘滞轮转。HTTP 代理、Relay、严格模式和代理池管理已录制。
- 图像、转录和 OpenAI、Groq、OpenRouter、DeepSeek、Perplexity Agent 普通聊天传输已用合成上游验证。官方 Anthropic API Key 接入 Wire，输入计数 gate 18/18。
- 数据库备份与恢复独立回放 14/14，验证配置恢复和保留用量。OIDC 在独立网络命名空间验证了本地测试颁发方、PKCE、签名、受众、nonce 和签发会话。
- 统计与图表独立 HTTP 回放 16/16，带合成数据库的 20 条读取与原版对比。统计 API 的 Key 分组 ID 哈希化，保留遮盖标签。用量 SSE 驱动活跃请求与结束事件，请求详情 JSON、流式更新、配置截断、保留上限和读取脱敏均已在两个入口验证。
- Linux 管理 TUI 7/7，实际终端验证 RTK 修改落库、退出恢复 PTY、隐藏到托盘和临时自启。管理台页面路由和合成配置操作通过 raw HTTP，未执行浏览器。
- Linux 已通过整二进制验证默认临时 HOME 的持久库、已验证进程接管、PID 清理及不碰无关监听。保留无显式主机配置时的回环监听。
- 原生 SAML 使用固定证书验证两层签名和受众。正常及不合格响应都已模拟。Rust 防止已验证 Assertion 重放，且无需浏览器带上 SameSite Cookie 就能关联待处理的本次请求。详见 [校验记录](research/saml.md)。
- Codex 推理已录制普通 Responses、Lite、Chat 转换、Web Search 回退正常传输、流内容量回退和暂态重试。Compact 使用当前请求的类型选路径，原版会滞后一条请求。两端差异已由 [传输合约](reference/codex-compact-contract.py) 验证。Gemini CLI Cloud Code Assist 的项目封装、JSON、SSE 和工具模式已通过模拟。
- Native OAuth 回放 23/23，覆盖 Gemini CLI、Claude、GitHub、Codex 授权与刷新，额外并发状态验证 19/19。Codex/Claude 额度读取 24/24。普通 Gemini 原生语音 13/13 对照，含 gzip 解码与首帧实时转发。
- RTK 支持四类工具输出形状，原版对照 23/23。Caveman、Ponytail、自定义提示和 Headroom 均通过模拟上游验证。Claude/OpenAI 的常用推理参数和 Claude 兼容历史也已录制验证。
- 已增加视频、搜索、抓取、System One、Translator 部分流程、代理池探测、Headroom 状态代理及多个 CLI 工具配置 API。所有凭据和媒体验证使用合成数据。视频连接断开不重发创建请求，带凭据的搜索不接受请求自行指定的主机。
- Fusion 的面板合成、单成员降级和失败路径已录制。Smart 通过本地 System One 验证锁定、置信度门槛、主动切换及逐轮选择。OpenCode 匿名 System One 与 Zen 密钥通道的公开用量由独立用例验证。语音、图像和 Web 组合已接管直连成员的顺序执行，语音和图像的粘滞轮转通过独立回放。
- GitHub Copilot 的 Chat、Responses 和 Claude Messages 均已完成共享模拟。模型首次收到匹配的 400 时切换至 Responses，后续直接走已学习的路径。Azure 直连已验证部署名、API 版本和请求头。Vertex 与 Vertex Partner 的 API Key、Bearer、RSA 服务账号和 ADC 通过 10 个独立对照场景。
- Ollama Local 已通过实测网关 JSON、工具历史与 NDJSON 流。原版使用 application/json 的 NDJSON 路径保持相同尾帧与用量，Rust 还支持标准 NDJSON MIME。标准语音目录和 Translator Send 鉴权、日志脱敏、Chat/Responses/Messages 直连分别通过独立合约。
- 定价 API、模型目录和可用状态、分页额度账号、语音列表、Headroom 进程、PXPIPE 日志和统计、Droid 设置已接入。管理台定价页面保存和重置通过 raw HTTP 验证。版本手动关闭接口已实际退出临时 Rust 实例，只清理本进程拥有的资源；现有系统服务未参与。
- OpenCode Go 与付费 Zen、AWS Bedrock/Bedrock xAI 和 Vertex 视频均通过独立原版对照。OpenCode 使用普通 TLS 和用户保存的付费密钥。Bedrock 支持 SigV4、EventStream、命名 Profile、递归 `source_profile` 和 SSO 缓存。Validate、Test 和批量探测用只读签名请求对照，覆盖 AccessDenied 的凭据判定。
- Gemini Live 通过合成音频验证设置确认、帧大小、部分结果、续传和计时，ElevenLabs Scribe 和 AssemblyAI 通过共享回放。ElevenLabs、Inworld、MiniMax 全球和中国区的 TTS 已录制成功字节和 JSON 封装。
- Linux 隔离环境已实测无 Node、无 shell 时启动和读取健康与设置路径。还需原版的路由在该模式返回 503。Cowork MCP 通过假进程验证会话隔离、帧限额与子进程清理。Tailscale 系统守护进程和已安装 Cloudflare CLI 的控制使用假程序验证，安装、自动恢复和旧版守护进程路径继续迁移。
- Cloudflared 安装器已核对 Linux、Windows 和 macOS 文件格式、重定向限制、取消和原子替换。Rust 独立运行时由 Rust 管理 Quick Tunnel，假进程验证了注册失败回滚、URL 更新、意外退出重启及网关重启恢复。带原版进程运行时沿用原版 Quick Tunnel 控制，避免两个进程争用同一配置。
- 模型目录同步已通过写入、并发、ETag 304、失败保留和进程重启对照。Cline/ClinePass 推理和模型发现、GitHub/ClinePass 额度读取、附加 Web 搜索 Provider，以及 SDWebUI、ComfyUI、本机 speech 入口通过独立模拟。Claude CLI 本地预热和常规 Anthropic API 的内嵌图像已纳入共享回放。
- Codex profiles、Claude、OpenClaw 和 Pi 的 CLI 配置接口以临时 HOME 验证。Claude 读取省略凭据，应用保留已有 token。OpenClaw 文件写入失败会回滚先前更新。CLI 页面验证使用 HTTP，未操作浏览器。
- 已验证 Claude Code 的 Anthropic API Key passthrough、常规 DeepSeek/GLM Coding/MiniMax Messages 传输、Kiro 标准模型发现与导入、CommandCode 和 Ollama Local embeddings。Claude Code 的缓存锚点、角色折叠和 Haiku thinking 降级已纳入共享回放。
- 独立 Rust 运行支持写入代理和组合策略设置，后台刷新通过提前窗口及代理边界用例。与原版同时运行时，通过旧版更新共享进程缓存。管理台出站代理测试通过隔离 HTTP 代理请求。
- Claude 和 Codex 的直连 OAuth 账号支持额度自动探测。日程计算通过 27 组、411 个时间点的原版对照。10 个隔离账号验证定时与滑动重置、流结束后写入记录、冷却及停用。两环境并行时由原版运行探测任务。
- Codex 重置积分和 Claude 重置额度已接入直连路径。Codex 检测到鉴权拒绝且成功更新 token 时使用原请求 ID 重试。额度对照 152/152，覆盖 GitHub、ClinePass、Gemini CLI、Kiro、Zed、CommandCode、DeepSeek、Synthetic、OpenCode Go/Zen、Groq、iFlow、Vercel AI Gateway、Ollama Cloud、Kimi、Cursor，以及 GLM、MiniMax、Qoder 的全球和中国区。十个直读 Provider 的代理用量接口有独立对照 5/5。合约使用合成凭据。
- VS Code Copilot、DeepSeek TUI、Grok Build、JCode、Hermes 和 Cowork 配置已在临时 HOME 验证。Hermes 支持命名配置、辅助模型、转录和已知端点批量更新。CLI 聚合状态由 Rust 返回，省略凭据。
- Kiro API Key 文本聊天通过 AWS EventStream 差分 8/8 和 18 条专项检查。Ollama 公共 NDJSON 入口、Codex STT 和远程图片预取通过隔离 HTTP 合约。图片预取只连接验证过的地址，并保留未下载成功的普通 Gemini URL 路径。
- Provider 请求头设置通过共享回放，覆盖别名、拒绝结构和鉴权头、OpenRouter 发送及清除后的恢复。管理台连接页可编辑请求头，并查看注册表默认值。
- OpenAI 的 Provider 级推理强度和用户请求字段优先级已通过上游对照。其他提供商沿用各自的推理参数转换。
- 推荐模型接口以 16 个独立观察点核对七类筛选，管理台可从九个已知目录添加模型。批量连接测试核对六种筛选和优先级，常规探测走 Rust，其余连接分别调用原版。
- GitLab PAT 和 Grok CLI 批量导入纳入共享回放。独立实例验证同一 GitLab 地址的凭据轮换、不同实例的账号分开存储、拒绝重定向、旧密钥脱敏和 Grok 重复导入。
- Cowork 的 MCP 注册表分页、缓存和工具元数据查询已纳入共享回放。隔离请求验证转发来源无法探测回环服务，直接本机会话可查询指定服务，无上游凭据转发。配置可保存 HTTPS 和本机 HTTP 连接器，保留独立配置和已有审批，重置仅清除当前管理的连接器。原版的本地 `browsermcp` 命名预设及开关已迁，使用实际回环端口，保存和重置仅接受直接本机来源。验证未启动浏览器。
- Headroom 支持查看、安装和移除代码及 ML extras。临时 Python 夹具验证命令白名单、并发拒绝、实时日志和失败状态，未修改本机 Python 包。
- Provider 新连接验证通过 35 条原版对照与 4 条专项检查。兼容节点、Azure、Ollama 和常规 Web、媒体及模型探测已经接管。专属 OAuth、签名、会话或特殊验证仍走原版。
- PXPIPE 已通过 103/103 条独立回放及无 Node 网关合约。Claude、Gemini Messages 及上游备用字体和计费配置支持压缩系统提示、工具文档、工具返回和已闭合的多轮历史，并处理 pin、计费标记及 Markdown 环境块。常规自定义配置通过图像像素核对，超出原生渲染资源上限的配置沿用兼容路径。
- Xiaomi MiMo API Key 和显式会话导入、Zed 手工导入、Kilo 免费模型目录已纳入共享回放。MiMo、Kiro、Cursor 和 CommandCode 的本机凭据文件以及 Linux Zed keyring 查询通过临时 HOME 验证，服务转发来源无法读取。本机 MiMo 自动识别读取 auth.json 的 API Key，桌面会话可手工导入。
- Provider 原生详情支持账号排序、批量测试、冷却清理、模型发现、Chat 和 Embeddings 探测、禁用与自定义模型、轮转设置。管理台用 raw HTTP 验证，未操作浏览器。
- Provider 账号表单覆盖 GitHub 设备授权、GitLab PAT、Grok CLI、MiMo、Zed、CommandCode、Cursor 和 Kiro。Kiro CLIProxyAPI 导入已进入共享回放，社交授权通过隔离 PKCE、状态校验、并发单次交换和错误脱敏。未登录真实 Provider。
- iFlow Cookie 导入已通过上游对照。认证状态先读取账户名，再向原服务申请刷新，接口遮盖返回的 Key，连接列表省略 Cookie。共享回放使用模拟服务。
- Codex 的 1455 回环授权代理已验证端口占用拒绝、来源与路径校验、授权码单次兑换及状态消费。Codex、Claude、Gemini CLI、Cline 和 ClinePass 的原生表单通过 Node VM 验证。回调页只向同源窗口或标签广播，凭据不写入浏览器持久存储。
- Kiro 已用本地 AWS EventStream 对照验证 API Key 及五类 OAuth 凭据。独立回放 29/29，覆盖工具参数分片、并行调用、保留同轮有效工具、工具结果、内嵌图片、Claude 和 GPT 推理参数，以及首轮缓存。用网关 API Key 开启显式会话时，上游会话 ID 按网关 Key 和 Provider 连接隔离。合成 HTTP 还验证了换用 AWS 端点时首轮不会重复注入。不规则输入仍有兼容路径。
- Cursor 公开模型列表、账号模型发现和组合预设通过 11 条 HTTP/2 对照，覆盖共享缓存、强制刷新、模型标记和静态回退。CommandCode 管理及公开目录与原版对照相符，公共目录请求不带连接凭据。真实 Cursor 账号的 TLS 协商和验收仍待验证。
- iFlow HMAC 签名 Chat JSON 和 SSE 已纳入共享回放，OAuth 兑换、API Key 读取及续期有隔离验证。GitLab 的标准 OAuth 和 PAT 均可接入，Chat 使用该连接保存的 GitLab 实例地址，并以本地合约验证同一来源。
- Zed 模型发现及多类上游的文本 JSON、SSE 通过租约更新、并发首帧和错误隔离对照。Grok CLI 动态目录和令牌续期支持 CONNECT、SOCKS5h，并按模型、OIDC 发现和令牌兑换地址分别应用显式绕过。隔离验证覆盖首个 401、过期、并发单次兑换、凭据脱敏、TLS 校验、重定向与断线后保留兑换记录。直连目录对照 11 项，直连续期对照 4 项。
- Grok CLI 推理通过原生 Responses 接入 Responses、Chat Completions 和常规 Anthropic Messages。请求改写的固定上游对照 13/13。回环网关验证 JSON、SSE、直连和隧道路由、401 与 503 后复用请求和会话 ID、过期续期、402 脱敏和代理失败隔离。会话按账号和调用 Key 隔离。兼容构建的 Grok CLI 目录续期也使用 Rust。真实 Grok 服务对 9Router 客户端标识的接受情况待测。
- Kiro Builder ID 和 IAM Identity Center 设备授权通过原版 HTTP 对照及隔离轮询验证。AWS 设备码和客户端密钥由 Rust 服务持有。Linux ARM64 在 QEMU 中以空 `PATH` 验证 CLI、健康检查、模型目录和版本接口。
- Linux Tailscale 安装使用已配置发行版软件仓库。模拟软件包管理器、sudo、systemd 和守护进程验证了错误密码、进度事件和授权 URL。现有系统服务未参与验证。
- Kilo Code 设备授权已验证轮询限速、单次保存、组织 ID 和后续带凭据 Chat。共享回放覆盖 JSON 和 SSE。GitHub、Kilo Code、Qoder 全球与中国区、Cursor 和 CodeBuddy 的常规 OAuth 连接检测已纳入回放。
- Nous 设备授权、API Key 和 OAuth 推理通过原版及隔离对照。密钥过期后至多并发刷新一次，支持 Portal 签发的 agent key 和有效的 OAuth access token。连接模型目录、API Key 验证与连接测试已纳入共享回放。OAuth 单账号测试和混合批量测试验证了刷新标记、续期与落库。
- Kimchi 模型发现与 Chat JSON/SSE 通过独立对照，覆盖账号缓存、固定推理地址、使用已保存凭据、代理目录和错误脱敏。xAI API Key Chat、Responses、OAuth PKCE、主动刷新和本机回调通过 6/6 独立对照，覆盖手动授权码、取消、会话状态和 Codex 回调回归。
- Qoder 全球和中国区设备授权通过隔离 PKCE、合法用户会话绑定、区域隔离和单次兑换检查。额度接口支持 OAuth 和 PAT 换取的 job token，原版对照覆盖额度明细、续期时间和缓存。真实服务验收待验证。
- Kimi 设备授权和续期通过原版对照及隔离验证，覆盖别名、设备注册绑定、并发单次续期和连接失败后的状态保存。共享回放覆盖 API Key、OAuth `/v1/chat/completions` 的 JSON 和 SSE、Messages 与 Responses。
- GLM 国际 Coding Plan 授权通过 3/3 独立回放，覆盖查找已有 API Key、创建、复制与授权失败。创建连接断开后消费本次授权，重新授权先查已有密钥。连接只持久化实际推理用的 Coding Plan Key 和账号资料。共享回放验证授权账户的 Messages。
- Windows 二进制在隔离 Wine 环境验证了 Win32 托盘、Unicode 登录自启快捷方式、退出和无桌面时的 HTTP 服务。macOS 的 AppKit 类型检查、LaunchAgent 文件与模拟 `launchctl` 流程通过。配置在下次登录生效，整应用交叉验证缺少 Apple SDK，实际登录启动待验证。
- Windows 终端菜单在隔离 Wine Console 验证了导航、退出、终端恢复和隐藏后同一进程继续服务。macOS 终端菜单通过 Apple 目标类型检查与模拟启动生命周期，实际 AppKit 运行待验证。
- Windows 和 macOS 的 Tailscale 安装使用各平台系统机制。Wine 验证未签名 MSI 拒绝、并发与清理，macOS 通过安装器类型检查和模拟验签。真实桌面安装及登录需要在目标平台确认。
- Linux 的独立 Tailscale 用户态进程使用网关自己的状态和 socket，启动、超时、Funnel 恢复与关停通过隔离验证，HTTP 回放 4/4。在已有系统守护进程时可继续使用其 scoped Funnel 配置。
- [Linux x86-64 原生预览包](native-package/dist/linux-preview.DXPzKbg8/9router-0.5.119-native-preview-x86_64-unknown-linux-gnu.tar.gz) 和 [Linux ARM64 原生预览包](native-package/dist/linux-preview.DXPzKbg8/9router-0.5.119-native-preview-aarch64-unknown-linux-gnu.tar.gz) 均用空 `PATH` 通过解包、摘要、CLI、健康检查和模型目录。x86-64 归档另用 Debian 的 glibc 2.36 通过同一检查，载入器提示存在 2.39 弱引用。ARM64 使用 QEMU，ELF 依赖 glibc 2.39。[签名升级](native-package/README.md#linux-preview-updates) 通过五组临时安装测试，覆盖版本验证、切换、回滚和中断恢复，实际发布源与公钥由部署方配置。
- Kimchi 的同源令牌回调、单次交换及资料回填通过独立对照。Muse 设备授权、签发和使用签发后的密钥完成模型目录、连接测试、Chat 和 Responses，在隔离流程中零回退。共享回放还核对了 Muse 支付 API Key、订阅鉴权头和加密推理字段原样往返。
- CodeBuddy 中国版设备轮询通过独立原版回放，覆盖授权域、并发、令牌保存与脱敏。国际版继续使用兼容路径，公开接口目前有供应商 SDK 的用户授权流程。
- CodeBuddy 中国版直连用量查询已验证循环额度和一次性额度。Windsurf 手工导入 `sk-ws-…` 密钥在隔离 SQLite 对照中保持固定端点和存储格式。Windsurf 的浏览器与 JWT 入口继续使用兼容路径。
- GitLab OAuth 和 PAT 的管理探测复用每条连接保存的实例，隔离验证了两种凭据和 Chat 请求只发往各自保存的地址。
- Zed 托管 Claude 已通过独立对照验证 OpenAI Chat、Responses 与 `/v1/messages` 的函数工具、内嵌图片、工具结果及 SSE。Responses 重放覆盖工具调用标识的继续使用。
- [Windows x86-64 原生预览包](native-package/dist/windows-update-preview.smn6C75O/9router-0.5.119-native-preview-x86_64-pc-windows-gnu.zip) 在隔离 Wine 环境中验证了归档、版本、帮助、CLI 查询、健康检查、模型目录、未配置签名源的更新接口及认证关停。实体 Windows 运行待验证。
- Bedrock 和 Bedrock xAI 的管理凭据探测已验证 HTTP CONNECT 与 SOCKS5h。签名后的 AWS 请求直接检查连通性和鉴权，代理失败不重试直连。
- Kiro OAuth 的 EventStream 推理支持受代理约束的续期。隔离 CONNECT 与 SOCKS5h 验证过期先续期、401 后只重发一次、请求体和会话 ID 相同、轮换令牌落库，以及代理关闭后停止续期。图片、工具、JSON 和 SSE 继续通过原有合约。
- Kiro 管理台模型发现的原版对照 11/11。公开目录与管理台共用直连、CONNECT、SOCKS5h 和按代理隔离的缓存。持有效 `us-east-1` 或 `eu-central-1` Profile 的 Builder ID、IAM Identity Center、Google、GitHub 和外部 IdP 可在过期或收到 401 后续期。法兰克福 IdC、Google、GitHub 和外部 IdP 的续期后目录与固定上游一致。隔离 TLS 验证了区域 Host 和 SNI、各自的令牌服务、并发单次兑换、拒绝不可信证书和断线后阻止重复兑换。真实 AWS 账号待测。
- Kiro 缺少 Profile 的已识别 OAuth 账号可先查询默认目录。续期时优先采用令牌响应中同区的有效 Profile。需要补查时先持久化已兑换的令牌，再按配置的出口查询并核对账号状态后写回。固定上游四组目录结果一致，TLS 合约另覆盖 Profile 查询失败后继续使用新令牌，以及保留并发修改的 Profile。
- Kiro 已保存的 OAuth 连接 Test 通过 11 条原版对照，隔离验证 6 个并发请求只发起一次续期。测试支持已保存的 Builder ID、IAM Identity Center、Google 和 GitHub 凭据。
- 阿里云 Coding Plan 系列的缓存块、商汤流式推理，以及 Xiaomi MiMo 云端、Token Plan 的区域传输已纳入共享回放。MiMo 云端 TTS 已校验 WAV、PCM 和 JSON，Token Plan 的音频接口返回与原版相同的未支持错误。
- 阿里云三类 Coding/Model Studio 连接的模型目录、Test 和 Validate，以及商汤两区连接 Test 已加入 12 条原版对照。
- Cloudflare Worker 中继管理 API 只创建新 Worker，使用各自的转发密钥。代理池列表脱敏，完整管理备份保存恢复所需的连接材料。本地对照 19/19，独立合约验证创建冲突、安全转发、备份恢复和表单凭据清理。未部署到真实 Cloudflare。
- Grok Build 周额度读取使用保存的 Bearer 与 gRPC-Web，对照 5/5。独立账号额度回归 152/152。真实上游接受情况待验证。
- CLI `--log` 及 `--skip-update` 已通过签名更新源的隔离检查，覆盖启动提示、日志脱敏和跳过启动检查后仍可手动查询版本。
- Translator 的 Gemini 输入转换和目标预览各通过 7 个上游对照场景，Anthropic 官方和兼容节点的预览通过 11 个对照场景。输入涵盖图片、并行工具、思考预算和参数边界，预览隐藏所选连接的密钥。Send 验证了 JSON、SSE、连接代理池、Relay 和严格代理失败。
- Gemini 模型发现已接入保存的连接密钥，符合原版 `models[]` 返回格式。管理台可用 API 返回的模型 ID 添加配置。七个接口检查点通过独立回放，另验了 HTTP 代理、Relay 和严格代理失败。真实 Google 服务待验证。
- OpenAI 和 Anthropic 兼容节点的公开模型发现与管理台共享原生出站选择。隔离 HTTP 验证直连、HTTP 代理、SOCKS5h、托管 Relay 与显式 NO_PROXY 的结果一致。管理台与公开目录保留各自的 Anthropic 上游路径。HTTPS CONNECT 被拒、代理配置无效和 Relay 目标不合格时没有重发到其他出口。公开目录过滤回显的连接密钥。
- 支持的 API Key 连接 Test 与批量 Test 共用 Rust 出站逻辑。隔离验证 OpenAI 和 Anthropic 兼容节点、Ollama Local、代理拒绝及 407 状态，并验证并发换密钥时旧测试结果不会写回。Anthropic 保留上游对 400 和 529 的判定。兼容节点与自定义 Embedding 的保存前 Validate 使用已存节点和全局代理，临时密钥只用于当前请求。
- Rust 已有的保存前 Validate 探测统一使用全局代理。GLM 两区、Kimi、MiniMax 两区、火山方舟和 BytePlus 新增原生校验，复用固定上游地址、默认模型和请求头。原版与 Rust 对照 49/49。隔离实测搜索、语音、Azure 和通用 GET→POST 流程，代理 407 或断连时不会追加 POST。
- OAuth 连接 Test 已接入 GitHub、GitLab、Kilo Code、Muse、Qoder 两区、iFlow、Kimchi、Grok CLI 和 Kiro 的原生代理路径。Cursor 与 CodeBuddy 两区使用本地令牌检查。隔离合约核对托管 Relay、GitLab 自托管代理、iFlow 查询参数和批测读回。Grok `/v1/user` 五种状态与原版一致，402 保持连接活跃并保存额度警告。隔离 CONNECT/SOCKS5h 验证 Grok 与 Kiro 的续期，小数格式的旧时间戳也能触发 Kiro 续期。
- Claude、Codex 和 Gemini CLI 的 OAuth Test 与写回由 Rust 执行。直连与固定原版对照 8/8。Claude 检查有效期并按需续期，Codex 保留 400 成功判定，Gemini CLI 检查 Code Assist 并在 401 后续期和重查。隔离 TLS 验证 CONNECT、SOCKS5h、按令牌端点单独配置的 NO_PROXY、拒绝连接以及错误脱敏。
- Kimi 和 Nous 的 OAuth Test 使用原生代理。Kimi 经 SOCKS5h 续期；Nous 经 CONNECT、按端点显式绕过或托管 Relay 测试、续期和发放 Agent Key。Nous 先持久化轮换令牌，再请求 Agent Key。隔离验证发放失败时仍保留新令牌和旧代理关闭后停止兑换。Kimi 与 Nous 设备授权、Nous 管理测试继续对照原版。
- Cline OAuth Test 回读用户端点并按需续期。原版直连对照 6/6，覆盖首次 401、过期续期和兑换失败。隔离 CONNECT、SOCKS5h 与显式 NO_PROXY 验证 WorkOS Bearer、续期写回和重查。
- Deno Deploy v2 API 以新建应用和生产部署实现中继，专用密钥存入管理员备份。原版回放 3/3，独立验证公网 IPv4 解析、实际 TLS 主机校验、中断或错误脱敏及备份恢复。Cloudflare 和 Deno 部署表单通过脚本合约。真实 Deno 部署待验证。
- Embeddings 已验证全局代理设置、no-proxy、账号代理池、Relay 和失败禁止直连。环回合约同时检查向量输入、凭据范围和上游错误脱敏。
- 图像二进制响应支持从上游返回的下载地址取图。原版和 Rust 对照了图像字节及响应头。Rust 使用固定的公网解析地址，遇到私网 URL、重定向或无效图像时保留上游 JSON 响应。
- 媒体连接支持 Vertex 服务账户、Cloudflare System One 账户 ID、自托管语音与向量服务地址，以及 Codex STT 和 xAI、Nous、Kimchi 图像理解的授权表单。19 个页面组合、5 组新增与编辑及脚本合约通过。没有打开浏览器或发起线上授权。
- Windows 签名版本发现通过隔离 Wine 的 5 组检查，覆盖版本比较、签名、平台标记及下载源限制。
- Windows 原生更新接口在隔离 Wine 通过 4 组验证，覆盖两个实际版本升级、摘要错误拒绝、版本不符回滚，以及 PE 和 SQLite 恢复分别中断后自动重试。实体 Windows 和实际断电待验证。
- 合入转录代理后，从 [Windows 预览归档](native-package/dist/windows-integrated.HjrG3DRd/9router-0.5.119-native-preview-x86_64-pc-windows-gnu.zip) 解出的 `0.5.119` 在独立 Wine 中对先前验证的 `0.5.120` 测试版本重过四组更新与恢复流程。
- Netlify 的受保护 Relay 部署、备份恢复和表单通过隔离验证。上游接口对照 2/2。函数使用按部署绑定的密钥，转发支持公网 IPv4，响应缓冲上限 4 MiB。云端验证待执行，需要账户提供 Secrets Controller。
- Vercel 创建独立项目，校验生产别名和 Relay 鉴权后保存连接。四种托管 Relay 的表单已通过脚本合约，部署与代理相关上游回放共 26/26。真实部署待验证。
- Bedrock 的命名 SSO 会话可使用仍有效的客户端注册续期过期令牌。原版和 Rust 回放各 2/2，本地验证两个网关并发只续期一次、原子写回、签名推理和失败时不使用旧令牌。旧式已过期缓存需要重新授权。
- Cloudflare Workers AI 图像通过 7 条独立回放，包含 JSON、图像和遮罩、直接图片字节、FLUX.2 multipart 和队列响应。模拟层保留原版文本表单字段并修正本地 Fetch 实现之间的 FormData 序列化差异。未调用真实 Cloudflare 图像服务。
- Stability AI 与 Hugging Face 的图像合约覆盖 8 种实际网关输出。Stability 使用官方 multipart，Hugging Face 托管 Fal 模型按官方客户端协议提交和查询，显式自托管端点继续使用原版 JSON 输入和二进制响应。已验证凭据和下载地址隔离，未调用真实服务。
- Fal 与 Black Forest Labs 的异步图像已通过隔离回放，覆盖文本、编辑、图片链接和二进制下载。Fal 使用官方宽高字段，BFL 用官方的 Ultra/Kontext `aspect_ratio` 及 Kontext `input_image`。模拟测试确认任务可能已受理后不会向其他账号或组合成员再次提交。结果下载不携带生成凭据。
- Runway 从图像入口生成 Gen-4 图像和 Gen-4 Turbo 视频，通过 4 个原版对照场景。隔离夹具核对了官方宽高、参考图、任务终态和重复提交防护。已退役的旧模型在提交前返回错误。
- Deno、Netlify 和 Vercel Relay 已在真实隔离 TLS 连接中验证仅 AAAA 的域名，检查全部 A/AAAA 结果后按字面地址连接，并校验原始域名。四种 Relay 的部署、探测与运行合约回归通过。
- 原版 MITM 模型映射的读取经过 5 条对照，覆盖本地权限、整表、按工具读取、URL 编码及空映射。使用合成 SQLite，既有网络和证书流程未进入此项回放。
- Fal 的直连 Validate 和保存连接 Test 通过 16 个 HTTP 场景和重定向拒绝。目录响应只用于对齐原版探测，生成权限需真实授权另验。
- macOS 签名版本发现接入了共同校验源，Apple Silicon 和 Intel 的 Rust 类型检查通过。完整构建仍需 Apple SDK 的 `TargetConditionals.h`，签名校验行为以本地隔离协议回放验证。
- [Linux x86-64 musl 预览包](native-package/dist/linux-musl-preview.EwNcANOS/9router-0.5.119-native-preview-x86_64-unknown-linux-musl.tar.gz) 和 [Linux ARM64 musl 预览包](native-package/dist/linux-musl-preview.EwNcANOS/9router-0.5.119-native-preview-aarch64-unknown-linux-musl.tar.gz) 经 ELF 检查均无动态依赖，含构建工具链许可文本。两份归档以空 `PATH` 通过解包、CLI、健康检查和模型查询，ARM64 由 QEMU 运行。
- 签名更新的管理台入口通过隔离 HTTP、Node VM 和独立回放。Linux 和 Windows 只在登录并发现已签名新版时显示安装动作，macOS 提示手动安装。
- Translator 可使用已保存的 Muse 订阅 Key 预览和发送请求。4 条独立回放通过，合约约束预览脱敏、上游域名及保存的凭据优先，并核对 JSON 与 SSE。
- Bedrock Chat 的原版直连回放 8/8。HTTP CONNECT 和 SOCKS5h 另验了真实隔离 TLS 上的 SigV4、JSON、EventStream 和 AWS 目标主机校验。代理支持已保存 AWS 密钥及可完全本机解析的静态 Profile，失败后不直接重试。
- Zed、Cline 和 ClinePass 的管理台与 `/v1/models` 已验证走 HTTP CONNECT 和 SOCKS5h。隔离检查覆盖真实 TLS、首个启用账号、手动模型覆盖、缓存以及失败后的静态目录。Cline 原版回放 10/10。
- Translator 的 Kimi OAuth `Send` 使用保存的设备凭据与默认 Messages 传输。4 条独立回放检查 JSON/SSE，另验证 401 后只用已更新连接重发、令牌轮换落库，并隐藏新旧令牌及预览中的设备标识。
- MiniMax Code 两区的设备授权和续期通过原版对照及隔离 HTTP、Node VM。已保存的是 `mcode-public` 的客户端授权，官方当前源码通过其受管网关推理。原生普通 MiniMax 路由使用 M Plan API Key。
- Translator 的结构化工具、OpenAI 和 Claude 图片输入已进入 5 条共享回放。OpenAI、Groq、OpenRouter、Vercel AI Gateway 的预览通过 8 个原版对照。标准 OpenAI 工具历史的目标预览也通过 HTTP 合约。
- Bedrock 验证了有效非严格代理的显式 `NO_PROXY`，并保留代理优先级。严格、无效或失败的代理禁止同次请求重发到直连。
- Zed 原生推理的直连、HTTP CONNECT 和 SOCKS5h 输出与固定原版一致，覆盖 Messages、Responses、Chat、JSON/SSE、工具和图片。代理拒绝时不会切换账号或嵌套 Combo 成员。
- Cline 和 ClinePass 推理的直连回放 10/10，HTTP CONNECT 与 SOCKS5h 的 JSON/SSE 与原版一致。代理下的 OAuth 推理支持过期续期和首次鉴权失败后单次重试。隔离合约核对了旧记录中的数字字符串时间戳、轮换后的凭据写回、代理关闭时结束重试，以及新旧凭据的脱敏。代理失败保持禁止直连及嵌套 Combo 重发。
- Responses 目标预览的函数调用和结果在开启及关闭流式时均与原版一致，预览隐藏保存的 Key。
- 同步 OpenAI 格式和 Gemini 图像验证了 HTTP CONNECT、SOCKS5h、显式非严格 `NO_PROXY` 和受保护的托管 Relay。19 条直连回放一致，代理失败和混合组合的专项检查没有重复提交。Gemini 经 Relay 使用认证头，图片结果下载不携带生成凭据。
- Zed Translator Send 的直连、CONNECT 和 SOCKS5h 在原版对照中保持 xAI JSON/SSE，回显的现有和轮换凭据都经过脱敏。原版 Zed 第 3 步预览返回 500，当前证据覆盖使用已转换请求体调用 Send。
- Cline 和 ClinePass Translator Send 已验证 JSON 信封、SSE，以及直连、CONNECT 和 SOCKS5h 下的同账号续期与终止语义。代理续期后的流式结果验证了新令牌脱敏。
- OpenAI、Gemini、ElevenLabs、Inworld、MiniMax 双区的单次语音合成均已验证代理。OpenAI/Gemini 直连回放 18/18，其他四个适配器原版 HTTP 回放 30/30，并核对了二进制、PCM/WAV、显式绕过和托管 Relay。
- OpenAI 格式、Gemini 批量、Deepgram 和 Hugging Face 的单次语音转录通过隔离代理回放，原版直连 24/24。验证了 multipart 字节、查询、认证、有效绕过、托管 Relay 边界和失败禁止重试。
- Fal 图像任务的提交、状态和结果共享同一路由。通过直连 3/3 及隔离 CONNECT、SOCKS5h 和托管 Relay 检查，包含轮询期间修改配置、失败不重发及路径凭据隔离。
- 原生网页搜索和抓取走统一代理选择。Brave 和 Tavily 验证了 CONNECT、SOCKS5h、托管 Relay、显式绕过和失败不重发。新增直连回放 7/7，额外搜索适配器回放 14/14。URL 查询携带 Key 的 GooglePSE 与 SearchAPI 在托管 Relay 前拒绝该请求。
- Runway 图像与视频入口使用同一个已选择的代理绑定完成提交和轮询。隔离验证了 CONNECT、SOCKS5h、托管 Relay、显式绕过和配置中途变更，直连四个原版输出一致。受保护的嵌套图像组合在上游提交前返回错误。
- Gemini 原生 `/v1beta` 语音的 JSON/SSE 经 CONNECT、SOCKS5h、托管 Relay 和显式绕过保持原有字节。直连错误、账号选择、压缩与首帧流式也同原版一致。
- xAI `/v1/videos` 的提交与轮询使用运行时选择的账号和代理，每次请求的错误和重定向会终止处理。原版直连 10 个结果一致。跨请求沿用连接 ID 提示，当前行为没有任务绑定表。
- MiMo 云端和自托管 TTS 验证了旧凭据格式、请求和音频字节。MiMo 可用托管 Relay，自托管隧道使用 HTTPS，本地 HTTP 可按有效非严格 `NO_PROXY` 直连。
- Fal 网关目录探测的代理路径通过 CONNECT 与 SOCKS5h 本地 TLS 结果对照。保存连接的 Test 使用连接及全局设置，未保存 Validate 仅用全局配置。
- OpenRouter 视频的提交和轮询另验了 CONNECT、SOCKS5h、有效绕过和托管 Relay，保留逐请求连接提示、单次提交与固定上游路径。xAI/Vertex 回归通过。
- AssemblyAI 的上传、提交和轮询验证了同一条代理路由、固定 HTTPS 目标和合法 CDN 地址。原版直连回放 7/7，受理或状态不明后保留请求终态，防止重复转录。
- BFL 的代理回放覆盖主域提交、区域域名轮询、配置中途变更和失败停止。CONNECT、SOCKS5h 与显式绕过输出一致，区域轮询范围用隧道连接。
- Gemini Live STT 通过了带认证的 CONNECT、SOCKS5h 和显式绕过测试。音频帧字节与 10 个原版直连场景一致，TLS 通过后才发送 Gemini 密钥，GoAway 沿用既有代理续连并避免重传音频。
- ElevenLabs 转录的 HTTP CONNECT、SOCKS5h、有效绕过和指定官方端点的托管 Relay 保留了 multipart 音频、转录分段与原始字幕。原版和独立 Rust 直连回放均为 4/4，代理失败不会重复上传。
- Codex 转录通过 HTTP CONNECT、SOCKS5h 和有效绕过对照 multipart WAV、账号标识、JSON 与纯文本。固定上游回放 18/18，零回退。代理路径发生故障时不会跨路由续期或重发。
- 组合成功修改、重命名和删除后重置相关轮转和会话粘滞状态，事务失败保留现状。原版和 Rust 对照的 12 次操作序列包含 23 次推理调用。
- System One 的普通决策和 Smart 分类请求支持生效的全局 CONNECT、SOCKS5h 与显式绕过。输入整形和结果与原版直连的 14 项快照一致，隔离 TLS 检查覆盖错误证书、上游重定向和失败停止。
- System One 按网关共用规则选择已保存的连接代理和代理池。隔离用例验证路由优先级、严格池的绕过限制、托管 Relay 的鉴权、公共 OpenCode 的固定代理池和故障后结束。直连 14 项快照重新对照一致。
- Headroom 在原生 Token Saver 中显示安装命令、运行状态和适用的启停操作。Node VM 验证复制命令与本机、外部、运行、受管等状态。
- 独立 Windows 版的 Headroom 启停、重启和接管使用 Win32 Job。实际 PE 在隔离 Wine 中通过并发启动、进程异常退出、网关重启后接管、子进程清理和陌生旧 PID 拒绝。实体 Windows 尚未验证。
- macOS Headroom 已接入每个数据目录专用 LaunchAgent。假 `launchctl` 隔离回放验证启停、并发、接管、提前退出和身份检查，Apple Silicon 与 Intel 的模块类型检查通过。整应用构建停在 `ring` 所需的 Apple SDK `TargetConditionals.h`，真实 `launchd` 登录会话待验证。[Apple LaunchAgent 文档](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
- Codex、Claude 的只读额度和 Codex 重置额度状态经 CONNECT、SOCKS5h 与有效绕过测试。原版和 Rust 的额度专项回放均为 152/152。受保护路径使用保存的访问令牌，提供商返回的客户端专属授权信息待真实服务验证。
- Gemini CLI 的额度读取可沿同一已选择的 HTTP CONNECT 或 SOCKS5h 路由完成项目发现和查询。隔离 TLS 验证代理变更、令牌过期、证书失败与重定向时的行为。普通无额度接入的 API Key 连接直接返回原版状态、内容和 `Vary`，专项 Rust 与上游 4/4 一致。
- Embeddings 在开启观测或本地允许匿名时继续走原生路径。原版与 Rust 通过 9/9 条专项回放，Rust 零回退。四条用量记录一致，受限密钥与转发请求保留鉴权。
- 开启观测时图像、语音合成和转录也走原生路径。九项 HTTP 对照覆盖 OpenAI 图像 JSON 和二进制、OpenAI/Gemini 语音、ElevenLabs 转录和字幕，Rust 回退为零。
- 原生 OpenRouter TTS 保留固定上游的 Chat Completions 音频 SSE 请求，按顺序拼接 Base64 音频并支持 WAV 和 JSON 输出。直连 5 项一致。隔离测试覆盖 CONNECT、SOCKS5h、显式绕过、托管 Relay 和错误中的凭据脱敏。[OpenRouter 音频输出文档](https://openrouter.ai/collections/audio-models)
- Ollama Local TTS 按已保存或默认主机发送 WAV/PCM，保留模型与音色解析。模型缺失时查询本机标签列表，特定音色错误时重试默认音色。上游与 Rust 的 11 项结果和 18 次请求一致，保护路径共用单一代理出口。
- Gemini 单条与批量向量请求通过 CONNECT、SOCKS5h、显式绕过和托管 Relay。受保护请求使用认证头，密钥不会进入 Relay 转发路径；检查覆盖错证书、重定向及混合凭据场景。[Gemini Embeddings 文档](https://aistudio.google.com/docs/embeddings)
- Vertex 与 Vertex Partner 的 Chat/SSE 已验证 CONNECT、SOCKS5h 和有效绕过。原版直连 10 个结果一致。托管 Relay 走无项目 ID 的 Gemini Key，服务账号和 ADC 保留直接授权流程。[Vertex Express API 文档](https://docs.cloud.google.com/gemini-enterprise-agent-platform/reference/express-mode/api-reference)
- Translator 的 Vertex/Partner Send 保留已转换请求体和原始 JSON/SSE。原版直连七项结果及出站顺序一致，验证了服务账号刷新写回、ADC 和 Partner 项目发现。连接保护覆盖 CONNECT、SOCKS5h、有效绕过和符合条件的托管 Relay。
- Translator 第三步的 Vertex/Partner 预览验证了 10 项上游对照，输出脱敏 URL、请求头和转换后的请求体。服务账号和 ADC 仅读本地连接元数据，验证中无出站请求及账号写入。
- Translator 第二步原生转换 Responses 的推理参数、摘要、不透明历史与自定义工具，也覆盖 Chat 和 Claude 的常用推理控制。11 项固定上游回放一致，Rust 回退为零。聊天转发仍保留对未能解析的续接状态的原有检查。
- MiniMax 全球与中国区的 API Key 额度查询保留原版主、备用端点，代理传输中两个端点共用 CONNECT、SOCKS5h 或有效绕过。Kimi API Key 额度也验证了隔离代理。额度专项原版和 Rust 各 152/152。
- Ollama Cloud 用量、账号与余额查询共用已选择的 CONNECT、SOCKS5h 或有效绕过。代理变更在下一次查询生效。隔离测试覆盖主机、重定向、证书和禁止直连回退，额度专项原版和 Rust 各 152/152。
- 独立 Translator 在 Rust 数据目录缺少记录时，可读取受信工作目录中旧版的 `logs/translator` 或 `.next/standalone/logs/translator`。保存仍写 Rust 数据目录。六项原版对照相同，验证过读取、覆盖优先级、文件缺失、非法 UTF-8 和不可信链接拒绝。
- Endpoint 页的 API Key 授权清单可搜索并勾选模型、别名和组合，保留手工 ID。远程管理台用会话读取存储目录，普通推理 Key 仍不能读该管理接口。实际 HTTP 与 Node VM 验证鉴权、禁用模型过滤及保存读回，动态目录探测次数为零。
- Linux 可在已保存的自管 Tailscale Funnel 离线时停用。停用取得私有守护进程租约并核对目标后清除启用意图，保留身份和归属。上游与 Rust 回放各 1/1，覆盖忙租约、目标不一致和无关 socket 保留。
- Provider 总览可对 OAuth、免费和 API Key 组手动批量测试，展示逐连接结果、耗时并读回连接状态。实际 HTTP、Node VM 和固定上游批量结果对照通过。
- 独立版 Cloudflare Quick Tunnel 可接管原版保存的可验证状态，保留短 ID 和地址。原版与 Rust 生命周期对照各 2/2，验证了停用、注册失败重试及拒绝不明状态。Linux 仅清理内核确认不存在的旧 PID，Windows 和 macOS 可恢复无旧 PID 标记的记录。
- 实际 Windows 发行 PE 在 Wine 中通过 Quick Tunnel 旧状态恢复、原短 ID 和地址、停用及无关进程保护。验证使用真实合成 Cloudflared 子进程与回环 Worker，原版回退为零。[Windows 切换验证](reference/windows-cloudflare-cutover-contract.py)
- 原生 Provider 管理台可创建、检查、编辑和删除 OpenAI Chat/Responses、Anthropic 兼容端点和自定义 Embedding 端点。校验接口对照固定上游 14 个返回与出站请求，测试密钥不写入连接。回放验证维度、编辑传播和删除清理。端点检查按本地权限和地址约束使用直连或有效绕过。
- 原生媒体组合页可设置成员轮转、试跑搜索、抓取、图像和语音示例，并读取当前组合用量。挂载页面验证了鉴权、持久化轮转和 TTS 返回字节，Node VM 覆盖其他样例与结果渲染。外部图像通过经校验的 HTTPS 链接打开。
- 原生 Profile 可保存与测试 OIDC/SAML、导入 IdP XML/PEM、下载 SP 元数据，并用密码确认备份导出和恢复。挂载页面与 Node VM 验证了真实本地测试颁发方、会话保护、空字段保留密钥及合成数据库恢复读回。SAML 说明覆盖固定上游的四组 IdP 配置，实时显示输入的 SP Entity ID。
- Cline 和 ClinePass 的管理台单模型与批量探测使用原生网关。对照了 9 种响应和 18 条批量结果，保留预热、并行与目录顺序。单模型探测已在隔离代理验证过期后的续期成功。成功续期前后的凭据在回显中均被遮盖，隧道失败不降级直连。
- ElevenLabs、Deepgram、Inworld、MiniMax 两区和 Edge TTS 的语音目录支持全局 CONNECT、SOCKS5h 与显式绕过。20 项直连快照一致，14 条独立回放零回退。ElevenLabs 缓存按凭据和出口区分，Edge TTS 缓存按出口区分。

## 第一轮结果

- 上游原版和 Rust 入口各通过 25/25 条 HTTP 回放。Rust 入口回退 19/25，见 [回放报告](app-cases/replay-report.md)。
- 健康检查由 Rust 响应，覆盖 GET、HEAD、OPTIONS。健康单元 gate 为 2/2。
- 直接回环请求的 Gemini 模型列表由 Rust 响应，含 `/v1beta/models` 和 `/api/v1beta/models`。完整 1,496 条列表以及 OPTIONS 与原版匹配，单元 gate 为 2/2。第一轮跨域和反向代理请求由原版鉴权；现已接入 Rust。目录提取只读取模型的 ID 和显示名。
- Claude 模型名 `[1m]` 解析单元 gate 为 25/25，等 Rust 聊天路由迁移后接入。

## 回放

在仓库根目录准备参考服务。原版构建需要 Node 22 与 npm，构建产物已在当前工作区生成。

```sh
npm install --no-package-lock --no-audit --no-fund
reference_build_data=$(mktemp -d -t 9router-build.XXXXXXXX)
DATA_DIR="$reference_build_data" NEXT_TELEMETRY_DISABLED=1 npm run build
cargo build --manifest-path rust/app/Cargo.toml
sh rust/reference/replay.sh
sh rust/reference/verify-accounts.sh
sh rust/reference/verify-usage.sh
```

[录制输入](reference/http-inputs.jsonl) 与 [参考启动脚本](reference/server.sh) 使用临时数据库、合成凭据和同一命名空间的本地模拟上游。更新用例后运行 `sh rust/reference/replay.sh record`，会合并独立 fixture，随后用 rust-anything 回放原版和 Rust。当前入口只绑定 `127.0.0.1`，未接入本机运行实例。录制归一化时间、UUID、新生成 Key 与 locale Cookie 到期时间，保留 Cookie 的 Max-Age 对照。

独立构建复用原版录制输入。测试进程单独启动模拟上游，Rust 网关使用空 `PATH` 和临时数据库，回放报告保存在另一目录。

```sh
cargo build --manifest-path rust/app/Cargo.toml --features native-package --locked
deck=$(mktemp -d -t 9router-standalone-deck.XXXXXXXX)
mkdir "$deck/rust"
cp -a rust/app-cases "$deck/rust/"
unshare -Urn -- sh -c '
  ip link set lo up &&
  rust-anything app replay "$1" --build "python3 $2/rust/reference/standalone-replay.py $2/rust/app/target/debug/server" --timeout 15' sh "$deck" "$PWD"
```

## 下一轮

继续补齐上游代码。用户要求剩余专用链路直接移植，以 Linux `cargo check` 验收。Trae、CodeBuddy 国际版、macOS 和 Windows 跳过专项测试。

- Kiro 任意未知的授权类型和缺少受保护出口必要信息的旧账号仍有限制。
- Bedrock 过期的旧式 SSO 仍需重新授权，固定上游的 Bedrock 调用也未提供需要 MFA 的回调。
- MITM 系统 DNS、证书信任及需要这些操作的恢复仍依赖管理员权限。IDE 实际联调待验证。监听只占用回环端口，启停只管理本实例。
- 上游已隐藏停用的 MiMo Free。网关和 Translator 保留 `mimo-free` Provider，推理请求直接返回 HTTP 410。小米官方代码记录的限免结束时间是 2026 年 7 月 26 日 18:00（北京时间）。[MiMoCode 限免截止定义](https://github.com/XiaomiMiMo/MiMo-Code/blob/v0.1.9/packages/opencode/src/util/free-api-sunset.ts)
- Translator 试发已接入 Trae 与 Grok CLI 原生推理，Grok CLI 会话按管理台调用方隔离。Kimi OAuth 试发复用原生代理和凭据续期，续期后只在出站路由一致时重试。本轮只做编译检查。
- Translator 试发已接入 Gemini CLI 的 Cloud Code Assist 请求、Codex 的 Responses 请求及上游内置 Antigravity，凭据续期使用保存的出站配置。MiMo Desktop 试发复用会话交换，云端试发支持从官方授权保存的 API Key。本轮只做编译检查。
- Translator 的 GitHub Copilot 与 Kiro 请求预览和试发已接入原生。Copilot 按模型选择 Chat、Messages 或 Responses，Kiro 复用凭据续期和 AWS EventStream 解码。验收为 Linux `cargo check`。
- Translator 已接入 Codex、上游内置 Antigravity、MiMo 和 Grok CLI 的请求预览。Codex Compact 和 Antigravity 非流式参数随预览保留到试发，MiMo 可预览区域会话或云端请求。Antigravity 封装请求到 OpenAI 的转换与现有 Rust 转换器共用实现。
- Translator 预览接入 Trae、Zed、Windsurf、Cline 和 ClinePass。Trae 会话、Zed 模型路由和 Windsurf 二进制封装仍由发送端生成，预览中的 OpenAI 请求可直接试发。Azure 和 GitLab Duo 已接入原生预览与试发，复用已保存的部署或服务地址与出站配置。
- Translator 已接入 iFlow、Ollama Local 和 CommandCode 的预览与试发。iFlow 复用请求签名，直连账号按需续期。Ollama Local 预览生成 `/api/chat` 请求，试发保留原始响应。CommandCode 复用会话封装和事件转换，重试上游的暂态流错误。
- Translator 已接入 Qoder 两区、Bedrock 和 Bedrock xAI、MiniMax 两区及 MiniMax Code 两区的预览与试发。Qoder 在试发时交换个人令牌并签名，Bedrock 在试发时签名并解码流式响应，MiniMax Code 复用 OAuth 续期、桌面租约及额度错误判断。
- Claude OAuth 接入原生网关 Messages 传输及 Translator 预览和试发。OAuth 续期沿用连接配置，凭据更新后核对推理出口再发送。Kilo Code 的预览和试发复用网关 URL、设备授权凭据和组织头。
- Translator 已接入 Nous、GLM、Kimchi 及 OpenCode Go 和 Zen 的预览和试发。Nous 按有效期选用代理密钥或访问令牌，续期后核对出站路由。GLM 复用 Messages 协议和设备授权生成的 Coding Plan Key，Kimchi 复用请求整形。OpenCode 按注册模型选择 Chat、Messages 或 Responses 协议，覆盖现有带 Key 的 Go 和 Zen 模型。
- OpenCode Free 的聊天推理要求 OpenCode 客户端。Rust 网关及 Translator 直接返回 HTTP 403，Zen 中列出的免费聊天模型使用相同规则。[OpenCode Inference API](https://opencode.ai/docs/console/api/inference/)
- xAI OAuth 文本接入 Translator 预览和试发，复用凭据续期并在重发前核对出站路由。
- xAI OAuth 网关聊天接入代理、凭据续期和鉴权拒绝时的重试。续期前后和发送前核对出站路由，代理故障按该账号返回，Translator 使用相同的代理选择约束。
- Translator 支持 Grok Web、DeepSeek Web、Perplexity Web、Gemini Web、ChatGPT Web、Muse Web 和 Cursor 的离线请求预览。预览保留可直接试发的 OpenAI 请求，凭据只显示掩码，上游会话和协议封装由试发时生成。
- Provider 设置、Profile 和优化页面的设置入口使用原生页面。Provider 详情的连接按钮跳转到当前页面的授权表单，兼容节点仍使用现有端点编辑器。
- CLI 集成页原生显示 Cursor、Roo、Continue、Amp、Qwen Code、Devin 和 OpenDesign 的上游配置指南。可复制所选路由地址、模型和配置，页面上的凭据预览使用掩码。CLI 设置链接使用原生页面。
- 媒体详情接入 Web Search、Web Fetch 和 System One 的原生试发。媒体预览可播放和下载音频、视频，显示并下载受支持的生成图片或二进制结果。
- 媒体 TTS 试发支持模型、语言分组和语音选择，复用 Rust 语音目录与固定上游的模型语音配置。选中的语音写入模型 ID。图片和视频试发可选择已保存的活跃连接，图片可选择二进制响应。
- Codex 图片试发在 JSON 输出模式下消费原生 SSE，显示生成进度、分段图片和最终结果。二进制模式仍可直接下载。
- 媒体页可为 Edge TTS、Google TTS、OpenCode System One 和 SearXNG 选择固定代理池。设置事务只更新所选 Provider，Edge 语音目录和合成共用出站选择。读取到旧的轮换配置时提示选择固定出口。
- SearXNG 接入原生 Web Search，目标由服务器的 `SEARXNG_URL` 设置指定，支持普通搜索、新闻、语言、时间和分页。
- 媒体详情原生显示服务地址、格式、额度与说明，并使用原生 Provider 账号入口。媒体 Combo 支持选择、添加、删除和调整成员顺序，仍可直接编辑 ID。媒体预览和 Combo 可复制 cURL。媒体列表、详情和 Combo 的页面链接均指向原生入口。
- Web Search 已接入 OpenAI、xAI、Kimi 和 MiniMax 的答案和引用解析。OpenAI 使用 Responses Web Search，xAI OAuth 使用与聊天一致的续期和出口校验。Kimi 搜索使用 Moonshot API Key。Vercel AI Gateway 沿用固定上游的 HTTP 400 结果，固定上游仅登记了搜索元数据。[OpenAI Web Search](https://developers.openai.com/api/docs/guides/tools-web-search?api-mode=responses)
- NVIDIA TTS 接入原生 FastPitch、Tacotron2 请求和 WAV 输出。AWS Polly 接入 SigV4、四种引擎、音频格式及媒体页 AWS 凭据表单。临时会话令牌不出现在管理接口响应中。[AWS Polly SynthesizeSpeech](https://docs.aws.amazon.com/polly/latest/APIReference/API_SynthesizeSpeech.html)
- Venice 和 TokenRouter 接入原生 Embeddings 和生图。Venice 向量接口接受文本输入，TokenRouter 沿用固定上游的 `.com` 地址。活动连接的模型发现按类型分类，并与固定目录合并。[Venice 兼容性](https://github.com/veniceai/api-docs/blob/main/guides/getting-started/openai-migration.mdx) · [TokenRouter 图像 API](https://tokenrouter-backend-api.tokenrouter.com/docs/how-to-use-image-models)
- ChatGPT Web 的 `gpt-image-2` 接入原生生图，复用已保存会话和临时聊天。每张图片以 PNG 返回，页面无法导出像素时使用当前显示尺寸截取图片。尺寸参数作为提示，代理隧道返回错误。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 公开的 `/landing` 由 Rust 提供。安装命令复制、路由示意、启动指南、管理台和项目文档入口随二进制打包。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 自定义 STT 模型的 `gemini-live` 支持 Provider ID 与别名。WebSocket 目标从连接保存的地址或 Provider 默认地址生成，CONNECT 与 SOCKS5h 使用同一目标主机和端口。Provider 详情可直接打开现有自定义模型表单。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- DeepSeek TUI 未显式请求 SSE 时使用非流式模式。Claude Code 请求按上游规则移除已有 MCP 替代项的重复工具，并清理历史中的失效工具引用。DeepSeek 模型的同名工具在多个聊天协议中保留首个定义。客户端标识不再单独触发旧版回退。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 指向 Nous Portal Chat Completions 的 OpenAI 兼容节点原生补齐归因标签，覆盖网关、Translator 预览和试发。Headroom 和出站代理可以在常规聊天中同时使用，压缩请求使用独立配置的服务地址，推理按 Provider 规则选择出站。Kiro 和 Zed 的 Headroom 组合仍待迁移。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Kiro 和 Zed 接入原生 Headroom。Kiro 在生成 `conversationState` 后压缩历史、当前轮次和工具结果的文本，保留原有工具 ID、参数和图片，再注入系统提示。Zed 在封装 `provider_request` 前使用现有 OpenAI 形状压缩。两者继续使用原有推理出站路由。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- CommandCode 在生成原生 `params` 后调用 Headroom。压缩结果只写回系统提示、消息文本和成功工具结果的文本。工具 ID、参数、推理块、图片和错误结果保留原值。可选系统提示在压缩后写入 `params.system`。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Kimi OAuth 聊天使用原生直连、显式绕过及 HTTP/SOCKS 代理。网关绑定推理地址和选定出口，续期后的推理重试再次核对二者，代理故障阻止自动直连。OAuth 令牌续期沿用 Kimi Portal 的出站配置。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Codex Responses、Compact 和 Gemini CLI 聊天走原生出站路由。OAuth 续期按令牌端点单独选择代理，推理发送及续期后的重试核对原推理地址和出口。支持直连、显式绕过及 HTTP/SOCKS 隧道。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- GitHub Copilot Chat、Responses 和 Messages 使用原生出站路由，分别绑定各自端点。Chat 切换到 Responses 时使用 Responses 的出站配置。GitHub OAuth 续期和 Copilot 会话交换按各自端点选路，续期后的推理重试复核目标出口。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- GitHub Copilot 的 Claude 模型接入网关 Messages 和无状态 Responses 请求，沿用 Copilot 上游 Messages 和现有 SSE 转换。模型思考参数、工具历史和内嵌图片经原生转换。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Codex 聊天接入原生远程图片预取，接受 Chat、Messages 和 Responses 图片输入，统一旧版 `image_url` 形状并内嵌下载结果。网关和 Translator 拒绝非公开地址，无法准备图片时返回 HTTP 502。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 无状态 Responses 的自定义工具声明、工具调用历史及推理摘要接入原生转换。跨协议的 JSON 和 SSE 输出恢复自定义工具调用，Chat 出站移除翻译元数据和不透明的连续性字段。服务端会话引用仍沿用支持它的上游 Responses 链路。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- OpenCode Go 和 Zen 的 Responses 工具输入在 Rust 中补齐调用 ID、按 UTF-16 上限截断 ID 和名称、过滤无名调用，并统一参数和工具结果的字符串格式。Chat 转 Responses 复用相同的截断和字符串转换。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Kiro API Key 的网关聊天和 Translator 试发使用同一套区域端点及原生 EventStream，支持 HTTP/SOCKS 代理、显式绕过和配置的 Relay。网关发送前复核端点与出口，Translator 预览支持已保存代理配置的 API Key。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Kiro OAuth 的网关和 Translator 支持按端点配置的显式绕过。推理请求按实际区域绑定出口，续期前后复核；令牌、资料发现及备用 CodeWhisperer 请求分别校验各自的代理配置。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 兼容向量节点缺少 `baseUrl` 时采用固定上游的 OpenAI 默认端点，Self-hosted Embedding 仍要求配置地址。向量请求识别已配置的 Relay 池，转发失败和重定向时保持原出站约束，错误消息对 Relay 令牌脱敏。Gemini 沿用要求 Relay 认证的路径。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- TTS、单次 STT、AssemblyAI 和 Gemini 原生 TTS 按实际选中的代理池处理旧版 Relay 标记。已认证且适用于供应商端点的 Relay 可原生转发，无法解析的旧配置直接返回路由错误，Gemini 原生 TTS 的错误消息已加入 Relay 令牌脱敏。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 同步生图、异步生图任务和 OpenRouter 视频通过实际代理池识别旧版 Relay 标记。BFL 提交与区域轮询复用同一个认证 Relay，轮询地址仍须匹配 BFL 区域域名及返回的任务 ID，结果和错误隐藏 API Key 与 Relay 令牌。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 网页搜索、网页抓取及带检索的模型搜索通过实际代理池识别旧版 Relay 标记。统一出站处理在截断错误摘要前隐藏 Relay 令牌，URL 中携带 Key 的请求沿用隧道要求。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Ollama Search 原生读取所选连接保存的 `baseUrl`，含复用 Ollama 账号的场景。保存地址按上游规则校验为公开 HTTP(S) 地址，发送仍使用对应连接的出站配置和凭据。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Serper、Brave、Exa、Tavily、Xquik、You.com、GLM、Google PSE、Linkup 和 SearchAPI 接入连接保存的 `baseUrl`。各适配器沿用自己的路径和认证，运行时校验保存地址，TinyFish 和模型联网检索沿用上游固定端点。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Zed 的 Messages 入口接受 Anthropic 版本头、Beta 头及常见 `cache_control` 位置，并沿用固定上游到 Zed 的请求转换。客户端 Anthropic 头和缓存标记在转换时由网关处理，Zed 托管端点使用自身请求头。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Zed 网关从转换前的请求读取 Claude Code 会话头、`metadata.user_id`、`prompt_cache_key` 等会话输入。缺少显式 ID 时按连接、调用方和历史前缀生成进程内稳定的线程 ID。转换后的模型请求移除仅供网关选取会话的字段。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Codex 和 Gemini CLI 从协议转换前的请求选取会话 ID，识别 Claude Code 会话头、元数据和已有缓存键。无显式 ID 时可按历史前缀生成稳定 ID，网关自动生成的 ID 按调用方隔离。Translator 预览和试发复用会话解析，Gemini CLI 试发补齐缺省 `sessionId`。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- CLI 配置页、接入指南和 Hermes 配置页共用命名端点预设。使用固定上游的浏览器存储键，可保存、选择和删除，读入当前网关、Cloudflare Tunnel 和 Tailscale 地址。应用主机配置后自动记住自定义地址。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- CLI 配置页的复制与应用操作使用相同的配置生成逻辑。复制时从当前选择取得网关密钥和工具选项，页面预览保留密钥掩码。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- CLI 配置页补齐 Codex、Kilo、Oh My Pi、OpenCode、Cline、OpenClaw、Droid、Pi、Crush、Forge、CodeWhale 和 Smelt 的配置读回。已有地址和模型预填到未编辑字段，Codex、Kilo、Oh My Pi 的状态新增仅含地址或模型的 `routerConfig`。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- OpenClaw 配置页可读回并编辑逐 Agent 模型，空值清除该 Agent 的 9Router 覆盖。复制和应用携带 `agentModels`。只提交默认模型时保留现有 Agent 覆盖，更新值与当前值相同时保留原模型对象。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Claude Code 管理台补齐 Fable 模型、`[1m]` 上下文标记和命名请求过滤。清空已编辑的档位移除对应环境变量，重置涵盖 Fable。复制输出可直接放入 Claude Code 的 `settings.json`，包含所选端点、模型档位及自动压缩窗口。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Codex 配置页补齐子代理模型的读回、编辑和应用。留空时使用主模型。预览和复制生成 `config.toml` 片段并转义字符串，预览显示密钥掩码，用户点击复制时使用所选密钥。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- OpenCode 配置页分别编辑启动模型和 Explorer 模型，读回已有选择。指定 Explorer 时将对应模型纳入配置。清空启动模型和删除模型后同步表单，预览与复制生成原生 `opencode.json`，预览显示密钥掩码。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 管理台补齐六种工具的手工配置导出。Cline 可切换 `globalState.json` 与 `secrets.json`，Kilo 输出 `auth.json`，Copilot 输出 `chatLanguageModels.json`，Cowork 输出 Gateway 配置，Droid 输出 `settings.json`，DeepSeek TUI 输出 `config.toml`。预览继续隐藏 API Key。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Grok Build 导出含子代理与上下文窗口的 `config.toml`。OpenClaw 导出默认模型、逐 Agent 覆盖与 Provider 注册。Jcode 分别导出 `config.toml` 与 `provider-9router.env`。Hermes 的 `.env` 复制使用选中的密钥。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- 代理池管理台补齐批量检测进度、上次检测时间和错误、清除所选。批量探测使用最多 10 个并发请求，刷新后可确认停用仍处于启用状态的失败项。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Basic Chat 在已有消息时切换模型会创建独立会话。图片可分批选择、逐张移除，在会话历史中显示预览。停止生成、请求出错和刷新中断后的状态可从历史读回。发送继续走已配置的网关鉴权。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Basic Chat 可按已连接 Provider 筛选和切换模型，支持自定义前缀、免授权 Provider 和组合。Rust 提供 Provider 标签，模型候选取自当前 Key 下的 `/v1/models`，切换 Key 时清除旧候选。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Veo AI Free 与 UC Web 被误用为文本模型时，网关和 Translator 直接返回对应的图像、视频端点提示。网关仍检查 API Key 和已启用的 Provider 连接，错误路径无需启动旧版进程或请求上游。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`。
- Gemini CLI 网关为缺少项目 ID 的旧 OAuth 连接原生调用 `loadCodeAssist`，必要时调用 `onboardUser`。项目发现沿该连接的出站配置，鉴权拒绝后尝试刷新令牌一次，写回前复核账户并保留并发写入的项目。网关、模型目录、额度与 Translator 共用已保存项目的解析。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`，未请求 Google 服务。
- 上游内置 Antigravity 的网关推理与 Translator 试发可为缺少项目 ID 的 OAuth 连接运行原生项目发现。Gemini CLI 和 Antigravity 共用 Code Assist 项目处理，Antigravity 在项目写回后复核推理出口，必要时沿原有 OAuth 刷新路径续期。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`，未请求 Google 服务。
- Zed 的 `/responses/compact` 和 `/codex` 别名复用原生 Responses 转换，按固定上游行为调用同一 Zed 推理端点。Responses 纯文本覆盖 OpenAI、Claude、Gemini 和 xAI；OpenAI 与 Claude 的工具和图片仍按模型声明校验。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`，未请求 Zed 服务。
- Codex、Gemini CLI 和 GitHub Copilot 的已启用 OAuth 连接只保存刷新令牌时，原生网关与 Translator 先取得访问凭据再发送推理。续期走现有出站规则，发送前复核原推理出口。Gemini CLI 网关在续期后还可发现缺失的项目 ID。两个 Linux 配置均通过 `cargo check --locked --offline`。
- xAI 视频创建和状态查询支持只有刷新令牌的 OAuth 连接。缺少访问令牌时先沿现有出站规则续期，核对视频请求出口后发送，沿用原订单的 Idempotency-Key 与账号绑定。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`，未调用视频服务。
- xAI 原生生图收到 401 或 403 后可续期并重发一次。重发要求已取得新凭据，且原请求出口在续期前后相同。两次请求的失败响应均隐藏凭据。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked --offline`，未调用生图服务。
- Coqui、Tortoise 和 PlayHT 的 TTS 已接入 Rust `/v1/audio/speech` 及语音组合。Coqui、Tortoise 使用本地免认证端点并解析语音参数，PlayHT 按上游拆分用户 ID 与密钥并支持代理。三者复用现有音频返回与 JSON Base64 处理。Linux 两种配置的 `cargo check --locked --offline` 均通过，未请求 TTS 服务。
- Codex 转录的直连、显式绕过和代理隧道共用 OAuth 续期流程。令牌和音频按各自端点选择出站，续期及 401 后重发时复核连接和音频出口。缺少 STT 适配器的 Provider 直接返回上游定义的 400 错误。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用转录服务。
- 聊天组合里的未知 Provider 在无活跃连接时以原生 404 交给组合策略继续处理。空组合返回原生 400。Codex 单模型请求保留 `[1m]` 给账号模型白名单，模型锁和上游请求沿用解析后的模型 ID。Linux 两种配置的 `cargo check --locked --offline` 均通过。
- Devin CLI 作为聊天组合成员时接入原生 ACP，沿用直连回环、非管理员进程、活跃 Key 和模型访问权限检查。单模型、模型别名和组合共用原生执行器。Linux 两种配置的 `cargo check --locked --offline` 均通过，未启动 Devin CLI。
- OpenCode System One、Edge TTS（含语音目录）、Google TTS 和 SearXNG 可按固定、轮询或随机方式选择活跃代理池，管理台可保存并读回模式。语音轮换使用 HTTP/SOCKS 池，轮询游标按 Provider 隔离。Smart 分类缺少适配器或凭据时，原生网关按会话绑定或成员顺序继续执行。Linux 两种配置的 `cargo check --locked --offline` 均通过。
- System One 公开入口和 Smart 分类接入 API Key 连接保存的自定义 `baseUrl`，即使对应 Provider 没有内置决策端点。请求沿现有出站规则，并验证目标地址、保留已有内置适配器的路径和请求头。Linux 两种配置的 `cargo check --locked --offline` 均通过。
- Embeddings 依实际选中的连接读取 API Key 或访问令牌，旧凭据类型和同一 Provider 下缺失凭据的账号不再触发整个请求回退。缺失凭据按原生账号失败处理，Ollama Local 保留免凭据转发，Copilot 继续使用专用令牌续期。Linux 两种配置的 `cargo check --locked --offline` 均通过。
- 网页检索和抓取原生接受旧 `api_key` 连接。历史凭据不兼容时，仅将实际选中的账号交给原生失败与轮换逻辑，同 Provider 或组合中的其他成员可继续。现有 Kimi、GLM、xAI 和 Antigravity 认证继续按各适配器处理。Linux 两种配置的 `cargo check --locked --offline` 均通过。
- Rust CLI 工具菜单显示已保存的端点与模型，包含 Codex 子代理、OpenCode Explorer 和 Hermes Profile 状态。授权码与设备码流程可将授权链接复制到剪贴板。组合菜单每次显示时读取最新数据，编辑只提交改动字段。Linux 两种配置的 `cargo check --locked --offline` 均通过，未运行交互菜单。
- Codex `/api/oauth/codex/exchange` 的旧访问令牌导入分支复用原生存储，支持读取旧式顶层账号和套餐声明，响应仅含连接摘要。管理台另提供单个访问令牌导入表单。Linux 两种配置的 `cargo check --locked --offline` 均通过，未提交真实令牌。
- 上游内置 Antigravity 的额度查询接入 Rust，读取订阅档位，区分免费与付费模型额度，解析 Argon、Gemini 和 Claude/GPT 的 5 小时及周窗口。短窗口按已耗尽的模型额度归并，周额度按凭据、项目和出口缓存，查询走连接的原生出站并可预先续期。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用 Google 服务。
- Antigravity 聊天收到 409/429 后按连接合并、限频刷新额度。账号选择避开模型和共享额度已耗尽的账号，采用上游重置时间并返回 `Retry-After`。额度仍显示可用或查询失败时，60 秒内连续三次触发 15 分钟的内存冷却，成功会清除连续失败记录。受保护的代理请求遵守原出站约束。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用 Google 服务。
- 管理台额度页显示订阅档位，支持按当前页最近重置时间排序、批量停用耗尽账号和启用已有可用额度的账号。停用账号也可查询额度，批量操作仅采用已读到的额度。Antigravity 家族的显示和隐藏复用原版分组规则。Linux 两种配置的 `cargo check --locked --offline` 均通过，未打开浏览器。
- Usage 概览补齐 Provider 实时活动、最近使用和错误状态，展示连接与免认证 Provider。趋势可在 Token、请求数和费用之间切换，Provider 与模型排行可分别按 Token 或请求数比较。Linux 两种配置的 `cargo check --locked --offline` 均通过，未打开浏览器。
- Usage 明细可展开模型、账号、API Key 和端点分组，按请求、最后使用时间和各用量指标排序。Token 与费用视图共用汇总，分项估算采用固定上游的 Token 比例规则，总额仍读取服务端记录。展开偏好保存在浏览器。Linux 两种配置的 `cargo check --locked --offline` 均通过，未打开浏览器。
- Usage 请求详情补齐 Provider 显示名、兼容旧字段的 Token 统计、TTFT、总耗时和 PXPIPE 压缩信息。时间筛选支持分钟并按浏览器时区换算。会话正文显示服务端遮盖状态。Linux 两种配置的 `cargo check --locked --offline` 均通过，未打开浏览器。
- 内置 Antigravity 的管理台模型发现接入固定上游的原生 POST 查询，沿连接出站并对返回内容脱敏。未提供模型列表的 Provider 直接返回原生 400。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用 Google 服务。
- Ollama Cloud 聊天接入网关与 Translator，使用官方 `/api/chat` 和保存的 API Key，复用 Ollama 消息、工具历史、JSON 与 NDJSON 转换。静态对照中，清单里 94 个包含 LLM 的 Provider ID 均匹配 Rust 分发分支（含停用状态处理）。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用 Ollama Cloud。

- Kiro 的 DeepSeek 3.2 和 Qwen3 Coder Next 按固定上游的模型规则移除图片、音频，覆盖网关与 Translator。Messages 和 Responses 请求的思考参数传给 Kiro 原生执行器，不重放历史推理块。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用 Kiro。

- Codex 重置积分与 Claude 重置额度原生接入路由选择，支持指定代理与显式绕过。Codex 鉴权拒绝后仍使用同一请求 ID 重试，Claude 的组织查询和提交保持相同的出站保护级别。中继可用于现有令牌的重置请求，OAuth 续期仍只支持直连、显式绕过及代理隧道。Linux 两种配置的 `cargo check --locked --offline` 均通过，未提交真实重置。
- Codex 积分查询和重置、Claude 额度重置的 OAuth 续期现可走配置的 Relay。复用账号锁、凭据写回和鉴权重试，Codex 重置重试保留请求 ID。Claude 重置后刷新该令牌在各出站路径的用量缓存。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Codex 和 Claude 的普通用量查询可通过配置的 Relay 执行，复用原生 OAuth 续期，出站改变时停止当前读取。Claude 的额度查询、旧版组织查询和缓存沿用选中的 Relay，缓存按 Relay 身份隔离。没有上游用量实现的 Provider 直接由 Rust 返回上游的提示。静态定位了固定上游 175 个 API 路径的 Rust 注册，含动态拼接路由。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Gemini CLI、CodeBuddy 中国版和国际版、MiniMax 两区、Kimi 以及 Ollama Cloud 的用量查询接入原生 Relay，支持供应商要求的 GET 和 POST。Gemini CLI、CodeBuddy 和 Kimi OAuth 账号沿已配置路线续期，Kimi 设备续期保留并发和轮换状态检查。MiniMax 两个额度地址须选择同一个 Relay，Ollama 的用量、身份和余额沿同一出口。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Kiro 和 Grok CLI 的用量查询接入原生代理、显式绕过和 Relay。Grok CLI 添加 REST 账单及订阅查询，解析月度、按需、预付和共享额度，REST 无额度时查询 gRPC-Web 每周额度。Kiro 的三个查询保留 GET、POST 和凭据类型，代理错误摘要隐藏 Relay 认证值。OAuth 续期可走代理隧道或显式绕过，Relay 查询仍要求现有有效令牌。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Kiro 和 Grok CLI 的用量查询可在同一 Relay 上执行 OAuth 续期，沿用账号锁、令牌轮换和持久化。Grok CLI 的 OIDC 发现与令牌请求、Kiro 的续期与账号 Profile 查询均核对 Relay 身份。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Grok CLI 和 Kiro 的网关聊天、Translator 试发、模型发现、已保存连接测试均接入原生 Relay。OAuth 续期后原路重试，Kiro 模型缓存按 Relay 身份隔离，目录和聊天错误摘要不回传 Relay 认证值。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Qoder 两区、Zed、Cursor、CommandCode、GitHub Copilot 和 ClinePass 的用量查询接入 Rust 原生代理与 Relay。Qoder 的 PAT 兑换和账号查询沿用同一出口，作业令牌缓存按出口隔离。GitHub Copilot 和 ClinePass 的 OAuth 续期沿配置出站。GLM 两区支持沿代理查询旧版 OAuth 连接的额度。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- 连接用量、Codex 积分查询与重置、Claude 额度重置的 Rust 路由移除旧版回退。DeepSeek、GLM 两区、Groq、OpenCode Go/Zen、Synthetic 和 Vercel AI Gateway 统一使用已有的原生额度出站。iFlow 用量、网关聊天和 Translator 的 OAuth 续期可使用配置的代理或 Relay，推理发送前复核该连接的出站路线。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- 管理台 Provider 模型查询、单连接测试和批量连接测试移除旧版回退。旧 Kiro 记录可用已保存访问令牌查询目录，目录不可用时原生返回提示。连接测试复用代理池预检，预检失败写回连接错误，并沿用原版零测试延迟的结果。旧 Kimi 记录缺少可用设备凭据时原生返回续期失败。Linux 两种配置的 `cargo check --locked --offline` 均通过，未运行实际探测。
- 公开模型列表、分类和单模型查询移除旧版回退。旧 Zed、Cline 和 ClinePass 记录按保存凭据原生查目录，单个供应商查不到时仍返回其他供应商与本地目录。Zed 和 Cline 系列的模型发现可走配置的 Relay，Zed 的令牌与模型缓存按 Relay 隔离。公开、管理台和组合选择器的 Cursor 动态目录接入现有原生代理及显式绕过。Linux 两种配置的 `cargo check --locked --offline` 均通过，未请求真实目录。
- Zed、Cline 和 ClinePass 的网关聊天与 Translator 试发接入配置的 Relay 和显式绕过。Zed 的组织查询、LLM 令牌换取与推理复用同一出口。Cline 系列的 OAuth 续期核对聊天 Relay 身份，续期后仍走原出口。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Cline、Codex、Claude、Gemini CLI 和 Antigravity 的连接测试支持配置的 Relay 与 OAuth 续期。Codex 和 Gemini CLI 管理台模型发现按原出口续期重试。Antigravity 原生订阅、模型额度和周额度查询支持 Relay，周额度缓存按中继身份隔离。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Codex 和 Gemini CLI 的网关聊天与 Translator 试发接入原生 Relay，OAuth 续期与鉴权重试沿同一中继，复用现有流式响应处理。Gemini CLI 网关的项目发现，以及上游内置 Antigravity 的聊天、生图、搜索、项目发现和 OAuth 续期沿配置的 Relay，并核对中继身份。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- 内置 Antigravity 管理台授权后，注册项目时沿保存账号配置的代理或 Relay，重试前复核出口。重新授权时，项目暂时查询不到会保留原项目 ID。注册完成返回新项目 ID 时，核对账号原访问令牌和项目后写回。订阅信息由原生额度接口实时读取。Linux 两种配置的 `cargo check --locked --offline` 均通过，未执行登录或请求真实上游。
- Zed 原生登录接入管理台和 OAuth 路由。Rust 生成 RSA 授权密钥，支持本机回调和手动粘贴 URL，解密后按会话单次写入账号，保留重新登录账号的代理配置。无效或旧回调保留当前登录，成功、取消或监听超时后释放本次私钥。Linux 两种配置的 `cargo check --locked --offline` 均通过，未执行真实登录。
- Grok CLI 的设备码申请、授权轮询和管理台入口接入 Rust。沿固定上游的设备码协议，轮询与当前登录会话绑定，遵守限频并单次保存账号、令牌到期时间和可用身份资料。Grok CLI 与 Muse 复用设备会话和轮询互斥。重新授权保留 Grok CLI 已配置的出口。Linux 两种配置的 `cargo check --locked --offline` 均通过，未执行真实授权。
- 通用 OAuth 路由移除旧版回退。设备流的 `authorize` 原生返回授权信息，设备码申请和轮询由各 Provider 的专用路由处理。通用路由原生返回未识别的动作、无效参数和不支持的流程。Linux 两种配置的 `cargo check --locked --offline` 均通过，未执行真实授权。
- 组合、系统设置和数据库备份恢复移除旧版回退。组合成员列为 NULL 或无法解析时按原版返回空列表，更新或删除后重置原生轮询。代理配置从 SQLite 生效，数据库恢复成功提交后重置轮询和唤醒原生额度定时任务。Linux 两种配置的 `cargo check --locked --offline` 均通过，未运行实际恢复。
- System One 公开决策接口移除旧版回退，与 Smart 分类共用 Rust 请求路径。按实际选中的账号读取 API Key 或访问令牌，支持原生 OAuth 续期的 Provider 沿配置出口续期并核对请求目标。HTTP/SOCKS 隧道、显式绕过和已认证 Relay 沿用现有出站约束。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实上游。
- Cloudflare 和 Tailscale 的七个管理路由移除旧版回退。无法核实归属的隧道返回冲突。Linux Tailscale 安装可读取同机保存的加密 sudo 密码，未知发行版返回自动安装不可用。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未启动隧道或执行安装。
- 向量、视频创建和视频轮询移除旧版回退。向量请求格式错误和配置读取失败由 Rust 返回。视频按选中账号处理，Vertex 服务账号通过令牌目标所配置的 HTTP/SOCKS 代理或显式绕过取令牌，视频请求再按自身目标的代理配置发送。xAI OAuth 令牌临期可提前续期，续期后复核视频出口。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未访问真实上游。
- 图片生成移除旧版回退。Rust 在展开组合前检查 API Key 权限，保留单层成员执行、能力排序和轮询。Cloudflare Workers AI、Stability AI 和 HuggingFace 接入 HTTP/SOCKS 代理、显式绕过与已认证 Relay。HuggingFace 队列轮询沿提交时的出口，隐藏供应商和 Relay 凭据。本地 SD WebUI、ComfyUI 使用直连且禁用重定向。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未访问真实上游。
- Translator 页面、历史加载保存、格式识别、转换、预览与试发移除旧版回退。OpenAI 中间格式复用工具声明和内容块归一化。兼容连接按保存的 `apiType` 构造 Chat 或 Responses，静态连接按各自传输格式预览并遮盖凭据。Devin CLI 试发复用既有的本机直连、网关 Key 和非提权约束。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未试发真实请求或启动 Devin。
- 聊天网关的 Chat、Messages、Responses、Codex、Gemini 和 Ollama 入口移除旧版回退。兼容 Provider 根据选中账号的 `apiType` 构造请求，识别保存的 API Key 或访问令牌。循环组合、未知 Provider 和不可转换的输入由 Rust 返回错误。PXPIPE 由已安装的 Rust 引擎处理，超出支持范围的请求保留原文。应用 Linux 两种配置和聊天请求整形回放工具均通过 `cargo check --locked --offline`，未执行回放或真实推理。
- 语音合成、转录和音色目录移除旧版回退。语音组合保留成员顺序、轮询和能力排序。转录按已选账号取凭据，未知自定义标记按 OpenAI multipart 处理并保留 Provider 认证头。将其他 Provider 的专用格式强制用于当前 Provider 会返回 400。音色目录沿选中账号的隧道或已认证 Relay 请求，缓存按出口隔离。Linux 两种配置的 `cargo check --locked --offline` 均通过，未调用真实语音服务。
- 搜索、网页抓取、模型别名、自定义模型、禁用状态、可用性和目录同步移除旧版回退。网页组合按成员处理无效 Provider，实际请求时才选账号，保留 Ollama 搜索复用聊天账号及出站限制。模型键保留字符串和数字写入，禁用目录按已存键查询，别名冲突与自定义模型更新沿 SQLite 事务处理。Linux 两种配置的 `cargo check --locked --offline` 均通过，未请求真实上游。
- 管理台页面、Provider 和兼容节点配置、用量、Key、代理池、CLI 设置、Headroom、PXPIPE、OAuth 导入和登录移除各自的旧版回退。媒体详情加载已保存的自定义向量节点，未知媒体、模型组合或 CLI 页面原生返回 404。兼容节点事务保留旧字符串格式的连接配置，无法安全读取的连接会拒绝更新。模型批量试跑使用原生目录和网关。PXPIPE 识别旧安装并启用 Rust 引擎。OIDC 发现、换令牌和签名公钥查询遵从已配置的 HTTP/SOCKS 代理或显式绕过，错误遮盖客户端密钥。Linux 两种配置的 `cargo check --locked --offline` 均通过。此时 `original::proxy` 只在生成的总路由中有调用。
- 应用总路由直接注册 rust-anything 的健康检查与 Gemini 模型列表单元。移除 Node 参考进程启动、旧 CLI 启动器和总路由回退，未注册的路由返回 404。原版仍可由独立参考脚本启动。Cloudflare 与 Tailscale 保留已存进程归属校验。后台 OAuth 续期跟随全局或账号级出站配置，Kiro、Grok CLI 与 Copilot 复用原生续期并保留并发锁，使用 Relay 时要求有效认证。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未运行回放、启停服务或访问厂商。
- 转录的旧自定义协议标记原生分发 Gemini 批量与 Live、Deepgram、HuggingFace、ElevenLabs、AssemblyAI、NVIDIA 和 OpenAI multipart。协议覆盖沿用所选 Provider 的基础端点和认证头，ElevenLabs 字幕选项由该 Provider 的配置决定。跨 Provider 使用 AssemblyAI 协议时，上传、提交和轮询沿配置的端点出站。自定义端点的代理路径禁止在提交 URL 中携带查询参数，任务 ID 和认证出口保持校验。Linux 默认与无默认特性配置的 `cargo check --locked --offline` 均通过，未调用真实转录服务。
- 按 Rust 路由注册和 HTTP 方法重新核对时，发现 CLI 引用的 `POST /api/auth/reset-password` 尚未注册。已补原生处理，要求有效 CLI 令牌或已认证的直接本机请求，只在 SQLite 事务中清除密码哈希。补齐后固定上游 175 个 API 路径及 290 个显式方法均有对应注册，含动态拼接和转发导出。固定上游与 Rust 的 145 个 Provider ID 集合完全一致。Linux 默认及无默认特性配置的 `cargo check --locked --offline` 均通过，未执行真实密码重置。
- Tailscale Linux 安装在系统包管理器报告缺包时，可为 APT 和 RPM 系列补配官方签名源并重试。APT 使用专属 keyring，RPM 启用元数据与包签名校验。Alpine、Void 和 Gentoo 使用各自的包管理器及服务管理器。登录复用现有短 ID，并可从 CLI 输出或守护进程状态取得授权链接。Linux 默认及无默认特性配置的 `cargo check --locked --offline` 均通过，未执行实际安装或启动服务。[Tailscale 软件源](https://pkgs.tailscale.com/stable/)
- 补齐上游重写规则的 `/v1/v1` 模型列表、详情和信息接口，沿用远程鉴权与模型可见范围。`/codex/*` 始终按 Responses 请求解析，Compact 只从已注册的 Compact 路径选取。Copilot 向量请求接入已认证 Relay，旧令牌被拒后在同一 Relay 续期，重发前核对绑定，错误信息隐藏凭据。Linux 默认及无默认特性配置的 `cargo check --locked --offline` 均通过，未请求真实上游。
- Copilot 的 Chat、Responses、Messages、Translator、公开模型目录和管理台目录支持已认证 Relay。Chat 切换到 Responses 时保持原出口，续期和重试复核 Relay 身份，目录缓存按凭据与出口隔离。Copilot 用量及配置为 System One 的连接也在续期时复核 Relay。Linux 默认及无默认特性配置的 `cargo check --locked --offline` 均通过，未请求真实上游。
- 兼容节点的推理、Translator、目录、向量及连接测试共用 URL 拼接，已保存的查询参数保留在 URL 上。无效配置由 Rust 返回错误。Anthropic 节点测试和推理使用同一个 Messages 地址，连接测试对 404 或重定向返回失败。自托管语音保留完整端点及 `/v1` 基址两种写法。新节点验证沿用原生直连和 DNS 固定策略。Linux 默认及无默认特性配置的 `cargo check --locked --offline` 均通过，未请求真实上游。
- 新节点验证接入 HTTP 和 HTTPS CONNECT、SOCKS5 和 SOCKS5h，以及显式绕过。模型列表、Chat、Responses、Anthropic Messages 和 Embeddings 的探测保持 DNS 固定及重定向禁用。隧道拨已验证的 IP，TLS 校验原目标域名。两种 Linux 配置均通过编译检查。

接入 MiMo 独立网页登录和 Grok Web 推理前，Linux 集成版通过 859/859 条 rust-anything 回放，旧版回退为零。本轮后续检查按用户要求止于 `cargo check`，隔离 `lift` 审计已停止。[此前回放报告](/home/wing/.cache/9router-native-ollama-mimo.8BeFvGyj/rust/app-cases/replay-report.md)

默认 Cargo 构建已启用 `native-package`，启动时直接使用 Rust 服务。Linux 默认配置和 `--no-default-features` 均通过 `cargo check --locked`。

rust-anything 1.5.1 的接线文件记录了健康检查和 Gemini 模型列表。本机工具副本通过显式资源列表和自定义回放命令暂存完整的本地依赖，保留 `reqwest` 和自定义 Router 状态。Linux 暂存版通过格式检查、Clippy、Cargo 测试和 859/859 回放，零回退。最终 lift 的注释检查报告 10 个文件里有 88 处 `unsafe`，其中 Windows 78 处、macOS 10 处。[隔离审计](/home/wing/.cache/9router-app-lift-audit-final.N5hWB2H7/final-lift-outcome.json)
