#![allow(dead_code)]

#[path = "../../app/src/provider_connections.rs"]
mod provider_connections;
#[path = "../../app/src/storage.rs"]
mod storage;
#[path = "../../app/src/storage_migration.rs"]
mod storage_migration;

mod chat_output {
    pub fn js_text(value: &serde_json::Value) -> String {
        value
            .as_str()
            .expect("fixture uses string identifiers")
            .to_owned()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let storage =
        storage::Storage::open(std::env::var_os("DATA_DIR").expect("fixture path").into())?;
    assert!(storage.root().is_dir());
    Ok(())
}
