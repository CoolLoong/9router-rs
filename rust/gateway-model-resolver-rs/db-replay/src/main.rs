#[path = "../../../app/src/model_resolution.rs"]
mod model_resolution;

use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().ok_or("missing model")?;
    let db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch(include_str!("fixture.sql"))?;
    if args.next().as_deref() == Some("anthropic") {
        db.execute(
            "INSERT INTO providerConnections VALUES ('anthropic', 1)",
            [],
        )?;
    }
    let info = model_resolution::get_model_info(&db, &model)?;
    let seats = model_resolution::get_combo_models(&db, &model)?;
    let exists = model_resolution::combo_exists(&db, &model)?;
    println!(
        "{}",
        json!({ "info": info, "seats": seats, "comboExists": exists })
    );
    Ok(())
}
