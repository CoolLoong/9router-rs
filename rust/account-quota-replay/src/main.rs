#[allow(dead_code)]
#[path = "../../app/src/cline_transport.rs"]
mod cline_transport;
#[allow(dead_code)]
#[path = "../../app/src/provider_connections.rs"]
mod provider_connections;
#[allow(dead_code)]
#[path = "../../app/src/provider_index.rs"]
mod provider_index;
#[allow(dead_code)]
#[path = "../../app/src/storage.rs"]
mod storage;

mod storage_migration {
    use std::path::Path;

    pub fn initialize(_: &Path, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(path.parent().expect("database directory"))?;
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../app/src/database-schema.json"))?;
        let db = rusqlite::Connection::open(path)?;
        for table in ["providerConnections", "settings"] {
            let sql = sqlite_schema_rs::port::build_create_table_sql(
                serde_json::json!(table),
                schema["tables"][table].clone(),
            )?;
            db.execute_batch(sql.as_str().expect("fixture table DDL"))?;
        }
        Ok(())
    }
}

mod dashboard_session {
    pub async fn authenticated(
        _: &crate::storage::Storage,
        headers: &axum::http::HeaderMap,
    ) -> bool {
        headers
            .get("x-replay-admin")
            .is_some_and(|value| value == "fixture")
    }

    pub async fn authorization_owner(
        storage: &crate::storage::Storage,
        headers: &axum::http::HeaderMap,
    ) -> Option<[u8; 32]> {
        authenticated(storage, headers).await.then_some([0; 32])
    }
}

mod chat_accounts {
    pub async fn active_connections(
        storage: &crate::storage::Storage,
        provider: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let provider = provider.to_owned();
        storage
            .run(false, move |database| {
                let mut rows = crate::provider_connections::read_all(database, Some(&provider))?;
                rows.retain(|row| row["isActive"] == true);
                Ok(rows)
            })
            .await
    }
}

mod provider_overrides {
    pub fn transport_headers(_: &serde_json::Value, _: &str) -> axum::http::HeaderMap {
        axum::http::HeaderMap::new()
    }
}

mod proxy_transport {
    #[derive(Default)]
    pub struct Clients;

    impl Clients {
        pub fn get(
            &self,
            address: &str,
            _: bool,
            _: std::time::Duration,
        ) -> Result<reqwest::Client, reqwest::Error> {
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(address)?)
                .build()
        }
    }

    #[allow(dead_code)]
    pub enum Route {
        Direct,
        Proxy { url: String },
        Relay { url: String },
        MissingStrict,
    }

    #[allow(dead_code)]
    pub enum Selection {
        Routed(Route),
        ExplicitBypass,
    }

    pub fn account_requires_tunnel(account: &serde_json::Value) -> bool {
        let details = &account["providerSpecificData"];
        details["connectionProxyEnabled"] == true
            || details["strictProxy"] == true
            || details["proxyPoolId"]
                .as_str()
                .is_some_and(|id| !matches!(id, "" | "__none__"))
    }

    pub fn tunnel_proxy_url(address: &str) -> Option<reqwest::Url> {
        reqwest::Url::parse(address).ok()
    }

    pub fn vendor_metadata_client(
        proxy: reqwest::Url,
        builder: reqwest::ClientBuilder,
    ) -> Option<reqwest::Client> {
        builder.proxy(reqwest::Proxy::all(proxy).ok()?).build().ok()
    }

    pub async fn vendor_metadata_proxy(
        storage: &crate::storage::Storage,
        settings: &serde_json::Value,
        account: &serde_json::Value,
        target: &str,
    ) -> Result<Option<reqwest::Url>, ()> {
        match select(storage, settings, account, target).await? {
            Route::Direct if !account_requires_tunnel(account) => Ok(None),
            Route::Proxy { url } => tunnel_proxy_url(&url).map(Some).ok_or(()),
            _ => Err(()),
        }
    }

    pub async fn select_with_bypass(
        storage: &crate::storage::Storage,
        settings: &serde_json::Value,
        account: &serde_json::Value,
        target: &str,
    ) -> Result<Selection, ()> {
        select(storage, settings, account, target)
            .await
            .map(Selection::Routed)
    }

    impl Route {
        pub fn relay(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            request
        }
    }
    pub async fn select(
        _: &crate::storage::Storage,
        _: &serde_json::Value,
        account: &serde_json::Value,
        _: &str,
    ) -> Result<Route, ()> {
        match account["providerSpecificData"]["proxyPoolId"].as_str() {
            None | Some("" | "__none__") => Ok(Route::Direct),
            Some(_) => Err(()),
        }
    }
}

mod provider_oauth {
    #[allow(dead_code, unused_imports)]
    mod replay {
        include!("../../app/src/provider_oauth.rs");

        static SERVICE: std::sync::OnceLock<refresh::RefreshService> = std::sync::OnceLock::new();

        pub(super) fn configure(base: &str) {
            let mut config = Config::pinned().expect("pinned provider metadata");
            config.codex.token = format!("{base}/codex/token");
            config.codex.client = "fixture-codex-client".into();
            config.claude.token = format!("{base}/claude/token");
            config.claude.client = "fixture-claude-client".into();
            config.github.token = format!("{base}/github/token");
            config.github.copilot = format!("{base}/github/copilot");
            assert!(
                SERVICE
                    .set(refresh::RefreshService::new(config).expect("refresh client"))
                    .is_ok()
            );
        }

        pub(super) async fn local_refresh(
            storage: &Storage,
            observed: &Value,
            mode: RefreshMode,
        ) -> Result<Value, RefreshError> {
            SERVICE
                .get()
                .expect("configured fixture refresh")
                .request(storage, observed, mode)
                .await
        }
    }

    pub(crate) use replay::kimi_portal;
    pub(crate) use replay::token_claims;
    pub use replay::{RefreshError, RefreshMode};

    pub fn configure(base: &str) {
        replay::configure(base);
    }

    pub async fn refresh_account(
        storage: &crate::storage::Storage,
        observed: &serde_json::Value,
        mode: RefreshMode,
    ) -> Result<serde_json::Value, RefreshError> {
        replay::local_refresh(storage, observed, mode).await
    }
}

#[allow(dead_code)]
#[path = "../../app/src/commandcode_identity.rs"]
mod commandcode_identity;
#[allow(dead_code)]
#[path = "../../app/src/cursor_identity.rs"]
mod cursor_identity;

#[allow(dead_code)]
mod account_quota {
    include!("../../app/src/account_quota.rs");

    pub fn replay(storage: Storage, base: &str) -> Router {
        let mut config = Endpoints::pinned().expect("pinned quota metadata");
        config.codex = format!("{base}/codex/quota");
        config.codex_credits = format!("{base}/codex/credits");
        config.codex_token = format!("{base}/codex/token");
        config.claude = format!("{base}/claude/quota");
        config.claude_legacy = format!("{base}/claude/organizations/{{org_id}}/usage");
        config.claude_settings = format!("{base}/claude/settings");
        config.claude_token = format!("{base}/claude/token");
        config.extra.clinepass = format!("{base}/clinepass/quota");
        config.extra.clinepass_refresh = format!("{base}/cline/api/v1/auth/refresh");
        config.extra.github = format!("{base}/github/quota");
        config.extra.github_refresh = format!("{base}/github/token");
        config.extra.copilot_refresh = format!("{base}/github/copilot");
        config.extra.gemini_quota = format!("{base}/gemini/quota");
        config.extra.gemini_assist = format!("{base}/gemini/assist");
        config.extra.zed_user = format!("{base}/zed/quota");
        config.extra.deepseek_balance = format!("{base}/deepseek/balance");
        config.extra.synthetic_quota = format!("{base}/synthetic/quota");
        config.extra.groq_models = format!("{base}/groq/models");
        config.extra.vercel_credits = format!("{base}/vercel/credits");
        config.extra.glm_quota = [format!("{base}/glm/global"), format!("{base}/glm/cn")];
        config.extra.ollama.usage = format!("{base}/ollama/usage");
        config.extra.ollama.identity = format!("{base}/ollama/me");
        config.extra.ollama.balance = format!("{base}/ollama/balance");
        config.extra.kimi_usage = format!("{base}/kimi/usages");
        config.extra.cursor.period = format!("{base}/cursor/period");
        config.extra.cursor.plan = format!("{base}/cursor/plan");
        config.extra.cursor.legacy = format!("{base}/cursor/legacy");
        for (endpoint, region) in config.extra.qoder.iter_mut().zip(["global", "cn"]) {
            endpoint.usage = format!("{base}/qoder/{region}/usage");
            endpoint.exchange = format!("{base}/qoder/{region}/exchange");
            endpoint.identity = format!("{base}/qoder/{region}/identity");
        }
        config.extra.minimax_usage = [
            [
                format!("{base}/minimax/global/token_plan/remains"),
                format!("{base}/minimax/global-alt/coding_plan/remains"),
            ],
            [
                format!("{base}/minimax/cn/coding_plan/remains"),
                format!("{base}/minimax/cn-alt/coding_plan/remains"),
            ],
        ];
        config.extra.kiro.codewhisperer = format!("{base}/kiro/cw");
        config.extra.kiro.amazon_q = format!("{base}/kiro/q");
        config.extra.commandcode.whoami = format!("{base}/commandcode/alpha/whoami");
        config.extra.commandcode.credits = format!("{base}/commandcode/alpha/billing/credits");
        config.extra.commandcode.subscriptions =
            format!("{base}/commandcode/alpha/billing/subscriptions");
        config.extra.opencode_go = format!("{base}/opencode/go");
        config.extra.opencode_zen = format!("{base}/opencode/zen");
        crate::provider_oauth::configure(base);
        build_router(storage, Reader::new(config).expect("fixture quota client"))
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let storage =
        storage::Storage::open(args.next().expect("fixture directory").into()).expect("storage");
    let endpoint = args.next().expect("loopback fixture endpoint");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener");
    println!(
        "ready {}",
        listener.local_addr().expect("local address").port()
    );
    axum::serve(listener, account_quota::replay(storage, &endpoint))
        .await
        .expect("fixture server");
}
