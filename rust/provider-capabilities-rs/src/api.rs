use serde_json::{Map, Value, json};

use super::{
    META, combo_inner, glob_matches, merged, object, promotion_vision, truthy, with_catalog,
};

pub fn model_capabilities(provider: Option<&str>, model: &str, catalog: Option<&Value>) -> Value {
    if model.is_empty() {
        return META["defaults"].clone();
    }
    let base = model.rsplit('/').next().unwrap_or_default();
    let provider = provider.unwrap_or_default();

    let provider_entry = if provider == "cmc" {
        "commandcode"
    } else {
        provider
    };
    let known = &META["provider"][provider_entry];
    if let Some(fields) = known.get(model).or_else(|| known.get(base)) {
        return Value::Object(merged(Some(fields)));
    }
    if provider_entry == "commandcode" {
        let lower = model.to_lowercase();
        let text_only = META["commandCodeTextOnly"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|entry| {
                let base = entry.rsplit('/').next().unwrap_or_default();
                lower == entry
                    || lower == base
                    || (lower.ends_with(base) && lower[..lower.len() - base.len()].ends_with('/'))
            });
        let mut all = merged(None);
        all.extend(object(&json!({
            "reasoning": true, "thinkingFormat": "commandcode",
            "thinkingEffortSupported": true, "vision": !text_only,
            "contextWindow": 1000000, "maxOutput": 384000
        })));
        return Value::Object(all);
    }

    let canonical = &META["model"];
    let known = canonical
        .get(base)
        .or_else(|| canonical.get(model))
        .or_else(|| {
            let lower_base = base.to_ascii_lowercase();
            let lower_model = model.to_ascii_lowercase();
            META["patterns"].as_array()?.iter().find_map(|entry| {
                let pattern = entry["pattern"].as_str()?;
                (glob_matches(pattern, &lower_base) || glob_matches(pattern, &lower_model))
                    .then_some(&entry["caps"])
            })
        });
    let mut result = merged(known);
    if !provider.is_empty()
        && let Some(catalog) = catalog
    {
        with_catalog(&mut result, provider, model, base, catalog);
    }
    if !result.get("vision").is_some_and(truthy) && promotion_vision(model) {
        result.insert("vision".into(), Value::Bool(true));
    }
    Value::Object(result)
}

pub fn service_kind_capabilities(kind: &str) -> Option<Value> {
    META["serviceKind"].get(kind).cloned()
}

pub fn combo_capabilities(
    models: &[Value],
    lookup: Option<&Map<String, Value>>,
    catalog: Option<&Value>,
    resolve: impl Fn(&str) -> Option<Value>,
) -> Result<Option<Value>, String> {
    combo_inner(models, lookup, catalog, &|id| Ok(resolve(id)), 0.0)
}
