# 迁移术语

| 原版术语 | Rust 侧含义 |
|---|---|
| Provider | 上游服务定义，含协议、认证和模型。 |
| Provider Connection | 一个 Provider 的凭据、偏好和健康状态。 |
| Provider Node | 用户自定义 Provider 及其接入地址。 |
| Provider alias | 客户端模型标识中斜杠前的名字。 |
| Combo | 具有路由策略或回退顺序的一组模型。 |
| Front door | `rust/app`，迁移期间的 HTTP 入口。 |
| Reference | 固定提交的原版 Node.js 服务，记录外部行为。 |
| Fallback | 由原版回答的请求。回放报告按录制用例统计。 |
