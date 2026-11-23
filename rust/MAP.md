# 9Router Rust 迁移

以 `1855aa44473e88feac9d3f131b9c0c5fc9706b68` 为上游基线，迁移网关、管理台、CLI 和 Provider 定义。[决策](DECISIONS.md) · [术语](GLOSSARY.md) · [当前交接](HANDOFF.md)

| 波次 | 单元 | 状态 |
|---|---|---|
| 0 | HTTP 原版记录和 Rust 入口 | 原版保留为离线对照。Rust 启动流程和总路由已经移除旧服务。 |
| 0 | `/api/health` | Rust 已接管。 |
| 0 | Claude 模型名 `[1m]` 标记解析 | Rust 单元通过 gate，已接入聊天。 |
| 0 | Gemini 模型列表 | Rust 已接管两个路径和入口鉴权。 |
| 1 | SQLite 初始化、登录要求查询、API Key 增删查改 | Rust 已接管初始化和读写。原版对照使用独立启动的参考服务。 |
| 1 | Provider 连接、兼容节点、组合和模型配置的 SQLite 管理接口 | Rust 已接管增删查改、凭据脱敏、连接调序、节点更新传播与删除清理。 |
| 1 | SQLite 数据迁移 | Rust 已支持配置 DATA_DIR、旧版列同步、迁移前快照和旧 JSON 导入。独立回放 5/5。 |
| 1 | 管理台登录签发、设置和全局鉴权 | 密码登录、OIDC、SAML、会话和设置均使用 Rust 处理。 |
| 1 | 数据库备份和恢复 | Rust 接管导出、事务化导入、版本信息和恢复后的定时任务更新。 |
| 1 | Provider 元数据、OpenAI 模型列表与单条模型查询 | Rust 静态目录的 145 个 Provider ID 与固定上游完全一致。已接管数据库配置、兼容节点 HTTP 自动发现、受限 Key 过滤和模型信息。Kiro、Grok CLI 的公开与管理台目录已验证 CONNECT 和 SOCKS5h。其他专属动态目录及代理续期继续迁。 |
| 1 | 模型解析和能力规则 | 已接入目录。解析器纯函数 gate 406/406，SQLite 解析回放 44/44，能力 gate 751/751，凭据脱敏 gate 21/21。 |
| 2 | 直连 OpenAI Compatible Chat 与账号调度 | Rust 已接管受限 Key 判断、请求整形、账号选择、同源重定向、同步输出、SSE、按模型冷却和用量写入。 |
| 2 | Anthropic Messages、OpenAI Responses 到 Chat 上游 | Rust 已接管非流式和 SSE 的文本、推理、常规工具调用及用量。 |
| 2 | 兼容 Responses 上游 | Rust 已接管 Responses 直通和 Chat、Messages 的同步及流式转换。 |
| 2 | Anthropic 兼容上游 | Rust 已接管 API Key 直连，覆盖 Messages 直通及 Chat、Responses 的同步和流式转换。 |
| 2 | Anthropic API 和输入计数 | 官方 API Key 直连已接管，输入计数 gate 18/18。 |
| 2 | Gemini API Key 聊天 | Rust 已接管生成 HTTP/SSE、常规工具调用、签名重放及网关 Gemini 原生文本入口。 |
| 2 | Grok CLI 推理 | 原生 Responses 支持 JSON、SSE 和代理 OAuth 续期。补齐了 Anthropic 思考与内联图片的请求转换和思考 SSE。真实供应商接入待测。 |
| 2 | Embeddings | Rust 已接管 API Key 兼容接口、Gemini 单条与批量嵌入，以及 Copilot OAuth 嵌入。Copilot 独立回放 18/18，覆盖直连、CONNECT、SOCKS5h 和有效绕过。 |
| 2 | 媒体请求与其他传输 | 已验证常用 TTS、STT、图像、视频、Gemini Live 和直连媒体组合。普通聊天图像预取已验证。专属传输及各 Provider 的边缘场景继续迁移。 |
| 2 | 默认 Provider 聊天传输 | 45 份配置驱动常规 Chat/Responses 路由，OpenAI、Groq、OpenRouter、DeepSeek、Perplexity Agent 已录制代表性请求。 |
| 2 | 组合执行和代理 | Rust 接管层叠 fallback、粘滞轮转、空响应识别、Fusion 与直连 System One Smart。音频、图像和 Web 组合支持直连，常规聊天支持 HTTP/SOCKS 代理和 Relay。特殊策略与连接继续迁移。 |
| 2 | 专属 Provider 凭据、OAuth 和实时用量 | 已录制 GitHub Copilot、Codex、Gemini CLI、Azure、Vertex、Ollama Local、Kiro、iFlow、Zed 等代表场景。用量历史、日汇总、事件和请求详情落库。剩余专属协议与复杂模型输入继续迁移。 |
| 3 | 网页管理台、CLI 与托盘、打包与升级 | Rust 已接管登录、Key、模型、组合、Provider 详情、Profile、Basic Chat、Usage、定价和常见 CLI 设置。已验证媒体组合试跑、Profile SSO 与备份恢复、Headroom 设置和九种工具的 CLI `show`、`connect`、`xai video` 与 Linux 托盘。跨平台启动和发布继续迁移。 |
| 4 | 独立运行、端到端回放、`app lift` | 已完成 Linux 独立构建回放。保留 macOS 和 Windows 实现，本轮跳过平台验证。剩余专属入口与 lift 见交接。 |

`rust-anything analyze . --whole-app` 的输出用于发现模块和依赖。它会漏掉部分 Next.js 别名导入和 JSX，迁每个单元前还需核对源代码。

现有的本机 9Router 服务不参与迁移录制，也不是当前部署目标。

## 完成条件

- 移植固定上游的网关端点、管理台流程、CLI 入口、Provider 定义和专属协议。
- 默认原生构建在 Linux 上使用 Rust 服务。已迁移项目的验收命令为 `cargo check --locked`。
- 按当前验收范围跳过逐项回放、`rust-anything app lift`、Trae 和 CodeBuddy 国际版专项测试、macOS 与 Windows 平台验证。
