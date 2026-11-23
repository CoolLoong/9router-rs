#[allow(dead_code)]
#[path = "../../app/src/cli.rs"]
mod cli;
#[allow(dead_code)]
#[path = "../../app/src/cli_connect.rs"]
mod cli_connect;
#[path = "../../app/src/droid_settings.rs"]
mod droid_settings;
#[allow(dead_code)]
#[path = "../../app/src/observed_response.rs"]
mod observed_response;

mod storage {
    #[derive(Clone)]
    pub struct Storage;
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
mod provider_connections {
    pub fn truthy(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(false) => false,
            serde_json::Value::Number(v) => v.as_f64() != Some(0.0),
            serde_json::Value::String(v) => !v.is_empty(),
            _ => true,
        }
    }
}
mod cli_tools {
    pub fn executable(name: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                std::fs::metadata(dir.join(name))
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
        })
    }
}

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("fixture listener");
    println!("ready {}", listener.local_addr().unwrap().port());
    axum::serve(listener, droid_settings::router(storage::Storage))
        .await
        .expect("fixture server");
}
