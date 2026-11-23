#![allow(dead_code)]

#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[path = "../../app/src/rtk_compression.rs"]
mod rtk_compression;
#[path = "../../app/src/rtk_filters.rs"]
mod rtk_filters;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut body: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
    rtk_compression::apply(&mut body);
    println!("{body}");
    Ok(())
}
