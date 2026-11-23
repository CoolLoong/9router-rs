#[allow(dead_code)]
#[path = "../../app/src/kiro_models.rs"]
mod kiro_models;
#[allow(dead_code)]
#[path = "../../app/src/provider_index.rs"]
mod provider_index;

mod provider_connections {
    use serde_json::Value;

    pub fn truthy(value: &Value) -> bool {
        match value {
            Value::Null | Value::Bool(false) => false,
            Value::String(text) => !text.is_empty(),
            Value::Number(number) => number.as_f64() != Some(0.0),
            _ => true,
        }
    }
}

use serde_json::{Value, json};
use std::io::{self, Read};

#[tokio::main]
async fn main() {
    let endpoint = std::env::args().nth(1).expect("fixture endpoint");
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .expect("fixture input");
    let accounts = serde_json::from_str::<Vec<Value>>(&input).expect("fixture accounts");
    let resolver = kiro_models::Resolver::new(Some(endpoint)).expect("fixture resolver");
    let mut output = Vec::new();
    for account in accounts {
        output.push(json!({
            "eligible":kiro_models::direct_account(&account),
            "models":resolver.discover(&account).await,
        }));
    }
    println!("{}", json!(output));
}
