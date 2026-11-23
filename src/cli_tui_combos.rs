use std::io;

use reqwest::Method;
use serde_json::{Map, Value, json};

use super::{Api, confirm, get_text, labels, menu, notice, pause, prompt, resource, rows};

pub(super) async fn run(api: &Api) -> io::Result<()> {
    loop {
        let combos = match api.get("/api/combos").await {
            Ok(data) => data,
            Err(error) => return notice(Err(error), "").await,
        };
        let combos = rows(&combos, "combos");
        let mut items = labels(&["← Back", "Create New Combo"]);
        items.extend(combos.iter().map(|row| {
            format!(
                "{} ({} models)",
                get_text(row, "name"),
                rows(row, "models").len()
            )
        }));
        match menu("🔗 Combos Management", "", &items).await? {
            Some(1) => create(api).await?,
            Some(index) if index > 1 => actions(api, &combos[index - 2]).await?,
            _ => return Ok(()),
        }
    }
}

async fn actions(api: &Api, combo: &Value) -> io::Result<()> {
    let Some(id) = combo["id"].as_str() else {
        return Ok(());
    };
    let path = resource("/api/combos", id);
    let items = labels(&["← Back", "Edit Combo", "Delete Combo"]);
    loop {
        let current = match api.get(&path).await {
            Ok(current) => current,
            Err(error) => return notice(Err(error), "").await,
        };
        let name = get_text(&current, "name");
        let header = rows(&current, "models")
            .iter()
            .filter_map(Value::as_str)
            .map(super::printable)
            .collect::<Vec<_>>()
            .join(" → ");
        match menu(&name, &header, &items).await? {
            Some(1) => edit_existing(api, id, &name, &current["models"]).await?,
            Some(2) => {
                if confirm(&format!("Delete combo \"{name}\"?")).await? {
                    notice(
                        api.request(Method::DELETE, &path, None).await,
                        "Combo deleted!",
                    )
                    .await?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

async fn edit_existing(api: &Api, id: &str, current: &str, models: &Value) -> io::Result<()> {
    let Some(name) = prompt(&format!("New name (Enter to keep \"{current}\"): "), false).await?
    else {
        return Ok(());
    };
    let mut changes = Map::new();
    if !name.is_empty() && name != current {
        changes.insert("name".into(), json!(name));
    }
    let mut selected = Vec::new();
    while let Some(model) =
        super::tools::choose_model(api, &format!("Add Model #{}", selected.len() + 1), true).await?
    {
        println!("✓ Added: {}", super::printable(&model));
        selected.push(model);
        if !confirm("Add another model?").await? {
            break;
        }
    }
    if !selected.is_empty() {
        let selected = json!(selected);
        if &selected != models {
            changes.insert("models".into(), selected);
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    notice(
        api.request(
            Method::PUT,
            &resource("/api/combos", id),
            Some(Value::Object(changes)),
        )
        .await,
        "Combo updated!",
    )
    .await
}

async fn create(api: &Api) -> io::Result<()> {
    let Some(name) = prompt("Combo name: ", false).await? else {
        return Ok(());
    };
    if name.is_empty() {
        return notice(Err("Combo name is required".into()), "").await;
    }
    let response = match api.get("/api/models").await {
        Ok(data) => data,
        Err(error) => return notice(Err(error), "").await,
    };
    let models = rows(&response, "models");
    let models: Vec<_> = models.iter().filter_map(model_id).collect();
    if models.is_empty() {
        return notice(
            Err("No models available. Please add providers first.".into()),
            "",
        )
        .await;
    }
    let mut items = labels(&["← Back", "Done"]);
    items.extend(models.iter().cloned());
    let mut selected: Vec<String> = Vec::new();
    loop {
        let header = format!(
            "Selected models ({})\n{}",
            selected.len(),
            selected.join("\n")
        );
        match menu(
            &format!("Select models for {name} (minimum 2)"),
            &header,
            &items,
        )
        .await?
        {
            Some(1) if selected.len() < 2 => {
                println!("Please select at least 2 models");
                pause().await?;
            }
            Some(1) => break,
            Some(index) if index > 1 => selected.push(models[index - 2].clone()),
            _ => return Ok(()),
        }
    }
    notice(
        api.request(
            Method::POST,
            "/api/combos",
            Some(json!({"name":name, "models":selected})),
        )
        .await,
        &format!("Combo \"{name}\" created successfully!"),
    )
    .await
}

fn model_id(model: &Value) -> Option<String> {
    if let Some(model) = model.as_str() {
        return Some(model.to_owned());
    }
    model["fullModel"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            Some(format!(
                "{}/{}",
                model["provider"].as_str()?,
                model["model"].as_str()?
            ))
        })
}
