#[allow(dead_code)]
#[path = "../../app/src/cline_transport.rs"]
mod cline_transport;
#[allow(dead_code)]
#[path = "../../app/src/kiro_auth.rs"]
mod kiro_auth;
#[allow(dead_code)]
#[path = "../../app/src/kiro_models.rs"]
mod kiro_models;
#[allow(dead_code)]
#[path = "../../app/src/provider_connections.rs"]
mod provider_connections;
#[allow(dead_code)]
#[path = "../../app/src/provider_index.rs"]
mod provider_index;
#[allow(dead_code)]
#[path = "../../app/src/storage.rs"]
mod storage;
mod proxy_transport {
    use crate::storage::Storage;
    use serde_json::Value;

    #[allow(dead_code)]
    pub enum Route {
        Direct,
        Proxy { url: String },
        Relay { url: String },
        MissingStrict,
    }

    impl Route {
        pub fn relay(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            request
        }
    }

    pub async fn select(_: &Storage, _: &Value, _: &Value, _: &str) -> Result<Route, ()> {
        Ok(Route::Direct)
    }
}
mod storage_migration {
    use std::path::Path;

    pub fn initialize(_: &Path, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(path.parent().expect("database directory"))?;
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../app/src/database-schema.json"))?;
        let sql = sqlite_schema_rs::port::build_create_table_sql(
            serde_json::json!("providerConnections"),
            schema["tables"]["providerConnections"].clone(),
        )?;
        rusqlite::Connection::open(path)?.execute_batch(sql.as_str().expect("provider DDL"))?;
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
}

#[allow(dead_code, unused_imports)]
mod provider_oauth {
    include!("../../app/src/provider_oauth.rs");

    pub fn replay(storage: Storage, base: &str) -> Router {
        let refresh =
            refresh::RefreshService::new(fixture_config(base)).expect("fixture refresh client");
        let fixture = axum::routing::post({
            let storage = storage.clone();
            move |axum::Json(data): axum::Json<Value>| {
                let refresh = refresh.clone();
                let storage = storage.clone();
                async move {
                    let mode = if data["rejected"] == true {
                        RefreshMode::Rejected
                    } else if data["background"] == true {
                        RefreshMode::Background
                    } else {
                        RefreshMode::Proactive
                    };
                    let observed = data["observed"].clone();
                    if data["cancel"] == true {
                        let waiter = tokio::spawn(async move {
                            refresh.request(&storage, &observed, mode).await
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(45)).await;
                        waiter.abort();
                        return axum::Json(json!({"cancelled":true}));
                    }
                    let result = refresh.request(&storage, &observed, mode).await;
                    axum::Json(match result {
                        Ok(account) => json!({"account": account}),
                        Err(error) => json!({"error": format!("{error:?}")}),
                    })
                }
            }
        });
        build_router(
            storage,
            OAuth::new(fixture_config(base)).expect("fixture client"),
        )
        .route("/fixture/refresh", fixture)
    }

    fn fixture_config(base: &str) -> Config {
        let mut config = Config::pinned().expect("pinned config parses");
        config.claude.client = "fixture-claude-client".into();
        config.claude.authorize = format!("{base}/claude/authorize");
        config.claude.token = format!("{base}/claude/token");
        config.codex.client = "fixture-codex-client".into();
        config.codex.authorize = format!("{base}/codex/authorize");
        config.codex.token = format!("{base}/codex/token");
        config.google.client = "fixture-google-client".into();
        config.google.secret = "fixture-google-secret".into();
        config.google.authorize = format!("{base}/google/authorize");
        config.google.token = format!("{base}/google/token");
        config.google.user = format!("{base}/google/user");
        config.google.project = format!("{base}/google/project");
        config.github.client = "fixture-github-client".into();
        config.github.device = format!("{base}/github/device");
        config.github.token = format!("{base}/github/poll");
        config.github.user = format!("{base}/github/user");
        config.github.copilot = format!("{base}/github/copilot");
        config.cline.authorize = format!("{base}/cline/authorize");
        config.cline.token = format!("{base}/cline/token");
        config.cline.user = format!("{base}/cline/user");
        config.cline.refresh = format!("{base}/cline/refresh");
        config.clinepass.authorize = format!("{base}/clinepass/authorize");
        config.clinepass.token = format!("{base}/clinepass/token");
        config.clinepass.user = format!("{base}/clinepass/user");
        config.clinepass.refresh = format!("{base}/clinepass/refresh");
        config
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("temporary fixture storage").into();
    let endpoint = args.next().expect("loopback fixture endpoint");
    let storage = storage::Storage::open(path).expect("storage");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener");
    println!(
        "ready {}",
        listener.local_addr().expect("local address").port()
    );
    let kiro = kiro_auth::build_router(storage.clone(), Some(endpoint.clone()))
        .expect("fixture Kiro client");
    axum::serve(
        listener,
        provider_oauth::replay(storage, &endpoint).merge(kiro),
    )
    .await
    .expect("fixture server");
}
