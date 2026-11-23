#[allow(dead_code)]
mod speech {
    use reqwest::Client;

    include!("../../app/src/speech_providers.rs");

    pub async fn run(data: &Value) -> Value {
        let id = data["provider"].as_str().expect("fixture provider");
        if data["mode"] == "registry" {
            return json!({"supported":Provider::resolve(id).is_some()});
        }
        let provider = if id == "deepgram" {
            Provider::Deepgram
        } else {
            Provider::resolve(id).expect("registered speech adapter")
        };
        let client = Client::builder()
            .no_proxy()
            .build()
            .expect("fixture HTTP client");
        let model = data["model"].as_str().unwrap();
        let result = match request_url(provider, data["endpoint"].as_str().unwrap(), model) {
            Ok(url) => {
                synthesize(
                    client.post(url),
                    provider,
                    data["input"].as_str().unwrap().trim(),
                    model,
                    data["apiKey"].as_str(),
                    false,
                )
                .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(Audio {
                bytes,
                format,
                base64,
            }) => {
                use base64::engine::general_purpose::STANDARD;
                let mut value =
                    json!({"status":200,"format":format,"bytes":STANDARD.encode(&bytes)});
                if data["responseFormat"] == "json" {
                    value["encoded"] = json!(base64.unwrap_or_else(|| STANDARD.encode(&bytes)));
                }
                value
            }
            Err(error) => json!({"status":502,"error":error}),
        }
    }
}

#[tokio::main]
async fn main() {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .expect("fixture input");
    let data = serde_json::from_str::<serde_json::Value>(&raw).expect("fixture JSON");
    println!("{}", speech::run(&data).await);
}
