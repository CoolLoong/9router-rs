# SAML 校验

使用 `saml-rs` 0.8.0 的兼容 API，沿用原配置中固定的 IdP 证书与 SP Entity ID。该版本支持 RustCrypto，并提供依据 AuthnRequest ID 校验 POST 响应的方法。仓库里的 SAML 配置没有单独的 IdP Entity ID，读取 Assertion 的 Issuer 只用于组装校验上下文，信任来自已配置的签名证书。原版要求 Response 和 Assertion 都签名，Rust 同样检查两层签名及受众、接收地址和时间。 [saml-rs 校验 API](https://docs.rs/saml-rs/0.8.0/saml_rs/raw/struct.ServiceProvider.html#method.parse_login_response_with_request_id)

Rust 记录待处理的 AuthnRequest，并将已签名的 Assertion ID 一次性入库。跨站 POST 没有带上状态 Cookie 时可从待处理记录关联。Metadata 使用当前 HTTP 请求的地址。本地对照里的 Next.js 将回环地址写作 `localhost`，两端示例使用同一个监听端口。复现见 [SAML 合约](../reference/saml-contract.py)。
