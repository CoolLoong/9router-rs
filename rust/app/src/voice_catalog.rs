use std::sync::OnceLock;

use icu_normalizer::DecomposingNormalizerBorrowed;
use serde_json::{Map, Value, json};

static CATALOG: OnceLock<Value> = OnceLock::new();

fn catalog() -> &'static Value {
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("voice-catalog.json")).expect("embedded voice catalog")
    })
}

pub(super) fn gemini() -> &'static [Value] {
    catalog()["gemini"]
        .as_array()
        .expect("embedded Gemini voices")
}

fn label(kind: &str, code: &str) -> Option<&'static str> {
    catalog()[kind][code.to_ascii_lowercase()].as_str()
}

fn language_name(code: &str) -> String {
    if let Some(name) = label("languages", code) {
        return name.to_owned();
    }
    let Some((lang, subtags)) = code.split_once('-') else {
        return code.to_owned();
    };
    let Some(base) = label("languages", lang) else {
        return code.to_owned();
    };
    let (mut name, mut rest) = (base.to_owned(), subtags);
    if let Some((script, next)) = subtags.split_once('-')
        && let Some(script_name) = label("languages", &format!("{lang}-{script}"))
    {
        name = script_name.to_owned();
        rest = next;
    }
    let qualifier = rest
        .split('-')
        .map(|tag| {
            label("regions", tag)
                .or_else(|| label("scripts", tag))
                .unwrap_or(tag)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name} ({qualifier})")
}

fn country_name(code: &str) -> String {
    if code.bytes().any(|byte| byte.is_ascii_lowercase()) {
        return code.to_owned();
    }
    label("regions", code).unwrap_or(code).to_owned()
}

fn order_key(value: &str) -> String {
    DecomposingNormalizerBorrowed::new_nfkd()
        .normalize(value)
        .chars()
        .filter(|ch| !matches!(*ch, '\u{300}'..='\u{36f}'))
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn generic(provider: &str, raw: &[Value], lang: Option<&str>) -> Value {
    let voices: Vec<_> = raw.iter().filter_map(|entry| {
        let voice = match provider {
            "local-device" => {
                let locale = entry["locale"].as_str()?.replacen('_', "-", 1);
                let language = entry["lang"].as_str()?;
                let country = entry["country"].as_str().unwrap_or_default();
                json!({"id":entry["id"],"name":entry["name"],"locale":locale,"lang":language,"country":country,
                      "countryName":country_name(country),"langName":language_name(language),"gender":entry["gender"]})
            }
            "elevenlabs" | "gemini" => {
                let labels = &entry["labels"];
                let locale = nonempty(&labels["language"]).unwrap_or("en");
                let lang = locale.split('-').next().unwrap_or_default();
                let mut voice = json!({"id":entry["voice_id"],"name":entry["name"],"locale":locale,"lang":lang,"country":"","countryName":"",
                                       "langName":language_name(lang),"gender":nonempty(&labels["gender"]).unwrap_or_default()});
                if let Some(category) = entry.get("category") {
                    voice["category"] = category.clone();
                }
                voice
            }
            _ => {
                let locale = entry["Locale"].as_str()?;
                let mut fields = locale.split('-');
                let lang = fields.next().unwrap_or_default();
                let country = fields.next().unwrap_or_default();
                let id = entry["ShortName"].as_str()?;
                let name = nonempty(&entry["FriendlyName"]).unwrap_or(id)
                    .replacen("Microsoft ", "", 1).replace(" Online (Natural) - ", " (");
                json!({"id":id,"name":name,"locale":locale,"lang":lang,"country":country,
                       "countryName":country_name(if country.is_empty() { lang } else { country }),
                       "langName":language_name(lang),"gender":entry["Gender"]})
            }
        };
        lang.is_none_or(|code| voice["lang"] == code).then_some(voice)
    }).collect();
    let mut groups = Groups::default();
    for voice in &voices {
        let lang = voice["lang"].as_str().unwrap_or_default();
        groups.add(lang, voice.clone(), false, false);
    }
    let value = groups.response(None, false);
    json!({"voices":voices,"languages":value["languages"],"byLang":value["byLang"]})
}

pub(super) fn specialized(provider: &str, entries: &[Value], lang: Option<&str>) -> Value {
    let mut groups = Groups::default();
    for entry in entries {
        match provider {
            "elevenlabs" => {
                let primary = nonempty(&entry["labels"]["language"]).unwrap_or("en");
                let verified = entry["verified_languages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| nonempty(&item["language"]).filter(|lang| *lang != primary));
                for code in std::iter::once(primary).chain(verified) {
                    groups.add(code, json!({ "id":entry["voice_id"],"name":entry["name"],"lang":code,
                                           "gender":nonempty(&entry["labels"]["gender"]).unwrap_or_default(),
                                           "free_users_allowed":entry["category"] == "premade" || entry["is_owner"] == true }), true, false);
                }
            }
            "deepgram" => {
                let id = nonempty(&entry["canonical_name"])
                    .or_else(|| nonempty(&entry["name"]))
                    .unwrap_or_default();
                let fallback = nonempty(&entry["canonical_name"])
                    .and_then(|id| id.rsplit('-').next())
                    .filter(|code| !code.is_empty())
                    .unwrap_or("en");
                let gender = entry["metadata"]["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .find(|name| matches!(*name, "masculine" | "feminine"))
                    .unwrap_or_default();
                add_languages(
                    &mut groups,
                    &entry["languages"],
                    fallback,
                    id,
                    nonempty(&entry["name"]).unwrap_or(id),
                    gender,
                );
            }
            "inworld" => {
                let id = entry["voiceId"].as_str().unwrap_or_default();
                add_languages(
                    &mut groups,
                    &entry["languages"],
                    "en",
                    id,
                    nonempty(&entry["displayName"]).unwrap_or(id),
                    nonempty(&entry["gender"]).unwrap_or_default(),
                );
            }
            _ => unreachable!("provider already validated"),
        }
    }
    groups.response(lang, false)
}

fn add_languages(
    groups: &mut Groups,
    languages: &Value,
    fallback: &str,
    id: &str,
    name: &str,
    gender: &str,
) {
    let lang = languages.as_array().filter(|values| !values.is_empty());
    let languages = lang.map_or_else(
        || vec![fallback],
        |values| values.iter().filter_map(Value::as_str).collect(),
    );
    for code in languages {
        groups.add(
            code,
            json!({"id":id,"name":name,"gender":gender,"lang":code}),
            true,
            false,
        );
    }
}

pub(super) fn minimax(data: &Value, lang: Option<&str>) -> Value {
    let mut groups = Groups::default();
    for (category, label) in [
        ("system_voice", "System"),
        ("voice_cloning", "Cloned"),
        ("voice_generation", "Generated"),
        ("music_generation", "Music"),
    ] {
        for entry in data[category].as_array().into_iter().flatten() {
            let Some(id) = nonempty(&entry["voice_id"]).or_else(|| nonempty(&entry["voiceId"]))
            else {
                continue;
            };
            let name = nonempty(&entry["voice_name"])
                .or_else(|| nonempty(&entry["voiceName"]))
                .unwrap_or(id);
            let (lang, display) = if category == "system_voice" {
                (
                    id.trim()
                        .split_once('_')
                        .map(|(lang, _)| lang)
                        .filter(|lang| !lang.is_empty())
                        .unwrap_or("Custom"),
                    name.to_owned(),
                )
            } else {
                ("Custom", format!("{name} · {label}"))
            };
            groups.add(
                lang,
                json!({"id":id,"name":display,"lang":lang,"category":category}),
                true,
                true,
            );
        }
    }
    for group in groups.0.values_mut() {
        if let Some(voices) = group["voices"].as_array_mut() {
            voices.sort_by_cached_key(|v| order_key(v["name"].as_str().unwrap_or_default()));
        }
    }
    groups.response(lang, true)
}

pub(super) fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

#[derive(Default)]
struct Groups(Map<String, Value>);

impl Groups {
    fn add(&mut self, code: &str, voice: Value, dedupe: bool, literal_name: bool) {
        let group = self.0.entry(code.to_owned()).or_insert_with(|| {
            let name = if literal_name {
                code.to_owned()
            } else {
                language_name(code)
            };
            json!({"code":code,"name":name,"voices":[]})
        });
        let voices = group["voices"].as_array_mut().expect("voice entries");
        if !dedupe || !voices.iter().any(|item| item["id"] == voice["id"]) {
            voices.push(voice);
        }
    }

    fn response(self, lang: Option<&str>, custom_last: bool) -> Value {
        if let Some(code) = lang {
            let voices = self
                .0
                .get(code)
                .and_then(|group| group["voices"].as_array())
                .map(Vec::as_slice)
                .unwrap_or_default();
            return json!({"voices":voices});
        }
        let mut languages: Vec<_> = self.0.values().collect();
        languages.sort_by_cached_key(|group| {
            (
                custom_last && group["code"] == "Custom",
                order_key(group["name"].as_str().unwrap_or_default()),
            )
        });
        json!({"languages":languages,"byLang":self.0})
    }
}
