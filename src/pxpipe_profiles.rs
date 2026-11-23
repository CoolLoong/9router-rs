use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::PAGE_CHARS;
use crate::cli::js_space;
use crate::pxpipe_render::{Font, Style, renderable};

const DEFAULTS: &[&str] = &["claude-fable-5", "claude-opus-5-5", "gemini"];
const GPT_WIDTH: usize = 152;
const GPT_HEIGHT: usize = 1932;

static VARIANT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
static GEMINI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^gemini-([0-9]+)(?:\.([0-9]+))?").unwrap());
static CLAUDE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"claude-(?:[a-z]+-)?([0-9]+)(?:-([0-9]+))?").unwrap());
static FABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"fable-([0-9]+)(?:-([0-9]+))?").unwrap());
static SOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^gpt-(?:5\.6|6\.1)-sol(?:-|$)").unwrap());
static GPT6: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^gpt-6(?:[.-]|$)").unwrap());
static SMALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:gpt-5(?:\.[0-9]+)?|gpt-4\.1)-(?:mini|nano)|^o4-mini").unwrap()
});
static GPT5_POINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^gpt-5\.[0-9]").unwrap());

pub(super) enum Selection {
    NotSelected,
    Deferred,
    Native(Box<Resolved>),
}

#[derive(Clone, Copy)]
pub(super) struct Resolved {
    pub standard: Geometry,
    pub history: Geometry,
}

#[derive(Clone, Copy)]
pub(super) struct Geometry {
    pub columns: usize,
    pub max_height: usize,
    pub style: Style,
    pub price: Vision,
    pub page_chars: usize,
}

impl Geometry {
    pub fn hard_rows(self) -> usize {
        (self.max_height.saturating_sub(8) / self.style.cell().1).max(1)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Vision {
    Patch28 {
        high_res: bool,
    },
    Patch32 {
        multiplier: f64,
        cap: f64,
    },
    Tiles {
        base: f64,
        tile: f64,
    },
    Mpix(f64),
    Flat {
        tokens: f64,
        exact: Option<(f64, f64, f64)>,
    },
}

impl Vision {
    pub fn tokens(self, width: usize, height: usize) -> f64 {
        match self {
            Self::Patch28 { high_res } => patch28(width, height, high_res) as f64,
            Self::Patch32 { multiplier, cap } => {
                let count = width.div_ceil(32) * height.div_ceil(32);
                ((count as f64).min(cap) * multiplier).ceil()
            }
            Self::Tiles { base, tile } => {
                let (width, height) = fit_edge(width, height, width.max(height), 2048);
                let (width, height) = fit_edge(width, height, width.min(height), 768);
                base + (width.div_ceil(512) * height.div_ceil(512)) as f64 * tile
            }
            Self::Mpix(rate) => (((width * height) as f64 / 1_000_000.0) * rate)
                .ceil()
                .max(1.0),
            Self::Flat { tokens, exact } => exact
                .filter(|(w, h, _)| *w == width as f64 && *h == height as f64)
                .map_or(tokens, |(_, _, count)| count),
        }
    }
}

fn fit_edge(width: usize, height: usize, edge: usize, bound: usize) -> (usize, usize) {
    if edge <= bound {
        return (width, height);
    }
    let ratio = bound as f64 / edge as f64;
    (
        (width as f64 * ratio) as usize,
        (height as f64 * ratio) as usize,
    )
}

fn patch28(width: usize, height: usize, high_res: bool) -> usize {
    let (max_edge, max_tokens) = if high_res { (2576, 4784) } else { (1568, 1568) };
    let fits = |w: usize, h: usize| {
        let pw = w.div_ceil(28);
        let ph = h.div_ceil(28);
        pw.max(ph) * 28 <= max_edge && pw * ph <= max_tokens
    };
    if fits(width, height) {
        return width.div_ceil(28) * height.div_ceil(28);
    }
    let long = width.max(height);
    let ratio = width.min(height) as f64 / long as f64;
    let short = |dimension| ((dimension as f64 * ratio).round() as usize).max(1);
    let mut low = 1;
    let mut high = long;
    let mut best = 1;
    while low <= high {
        let middle = low + (high - low) / 2;
        if fits(middle, short(middle)) {
            best = middle;
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    best.div_ceil(28) * short(best).div_ceil(28)
}

pub(super) fn select(model: &str, scope: Option<&str>, raw: Option<&str>) -> Selection {
    let folded = model.to_lowercase();
    let base = VARIANT.replace_all(&folded, "");
    let leaf = base.rsplit('/').next().unwrap_or(&base);
    let scope = scope.unwrap_or_default().trim_matches(js_space);
    let allowed = |entry: &str| {
        let entry = entry.trim_matches(js_space).to_lowercase();
        !entry.is_empty()
            && [&*base, leaf].iter().any(|candidate| {
                *candidate == entry
                    || candidate
                        .strip_prefix(entry.as_str())
                        .is_some_and(|tail| tail.starts_with('-'))
            })
    };
    let selected = if scope.is_empty() {
        DEFAULTS.iter().any(|entry| allowed(entry))
    } else {
        !matches!(
            scope.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | "none"
        ) && scope.split(',').any(allowed)
    };
    if !selected {
        return Selection::NotSelected;
    }
    let overrides = parse_overrides(raw);
    let custom = overrides
        .iter()
        .filter(|(name, _)| base.starts_with(name.as_str()) || leaf.starts_with(name.as_str()))
        .fold(None, |best: Option<(&String, &Value)>, item| {
            if best.is_none_or(|(prior, _)| {
                prior.encode_utf16().count() < item.0.encode_utf16().count()
            }) {
                Some(item)
            } else {
                best
            }
        });
    let gemini = gemini_profile(leaf);
    let misresolved = base.contains("gemini") && !gemini
        || base.contains("grok") && !base.starts_with("grok-") && !leaf.starts_with("grok-")
        || base.contains("qwen") && !base.contains("qwen3.8-27b");
    if custom.is_none() && misresolved {
        return Selection::NotSelected;
    }
    let resolved = if gemini {
        built_geometry(
            312,
            728,
            Style::default(),
            Vision::Flat {
                tokens: 1120.0,
                exact: Some((1568.0, 728.0, 1078.0)),
            },
        )
    } else if let Some((name, config)) = custom {
        overridden(builtin(name).0, config)
    } else {
        let (qualified, recognized) = builtin(&base);
        if recognized {
            qualified
        } else {
            builtin(leaf).0
        }
    };
    if [resolved.standard, resolved.history]
        .iter()
        .all(|geometry| renderable(geometry.columns, geometry.max_height, geometry.style))
    {
        Selection::Native(Box::new(resolved))
    } else {
        Selection::Deferred
    }
}

fn parse_overrides(raw: Option<&str>) -> Map<String, Value> {
    let value = raw
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);
    let values = match value {
        Value::Object(values) => values,
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(i, value)| (i.to_string(), value))
            .collect(),
        _ => Map::new(),
    };
    values
        .into_iter()
        .filter(|(_, value)| value.is_object() || value.is_array())
        .map(|(name, config)| (name.to_lowercase(), config))
        .collect()
}

fn builtin(model: &str) -> (Resolved, bool) {
    let mut style = Style::default();
    if model.contains("claude") || model.contains("anthropic") {
        let dense = FABLE.captures(model).is_some_and(|parts| {
            parts[1].parse::<u64>() == Ok(5)
                && parts.get(2).is_none_or(|part| {
                    part.as_str().len() > 2 || part.as_str().parse::<u64>() == Ok(0)
                })
        });
        let high_res = CLAUDE_VERSION.captures(model).is_none_or(|version| {
            let major = version_number(&version[1]);
            let minor = version
                .get(2)
                .map_or(0, |part| version_number(part.as_str()));
            major > 4 || major == 4 && minor >= 7
        });
        style.row_spacing = if dense { 0 } else { 2 };
        let mut result = built_geometry(312, 728, style, Vision::Patch28 { high_res });
        result.history.style.row_spacing = style.row_spacing;
        return (result, true);
    }
    if SOL.is_match(model) {
        style.font = Font::JetBrains14;
        return (
            built_geometry(
                84,
                1954,
                style,
                Vision::Patch32 {
                    multiplier: 1.0,
                    cap: f64::INFINITY,
                },
            ),
            true,
        );
    }
    let vision = if GPT6.is_match(model) {
        Vision::Patch32 {
            multiplier: 1.2,
            cap: f64::INFINITY,
        }
    } else if SMALL.is_match(model) {
        Vision::Patch32 {
            multiplier: if model.contains("nano") { 2.46 } else { 1.62 },
            cap: 1536.0,
        }
    } else if GPT5_POINT.is_match(model) {
        Vision::Patch32 {
            multiplier: 1.0,
            cap: 10_000.0,
        }
    } else if model.starts_with("gpt-5") {
        Vision::Tiles {
            base: 70.0,
            tile: 140.0,
        }
    } else if model.starts_with("o1") || model.starts_with("o3") {
        Vision::Tiles {
            base: 75.0,
            tile: 150.0,
        }
    } else if model.starts_with("grok-") || model.contains("qwen3.8-27b") {
        style.font = Font::JetBrains14;
        return (built_geometry(84, 512, style, Vision::Mpix(1000.0)), true);
    } else {
        return (
            built_geometry(
                GPT_WIDTH,
                GPT_HEIGHT,
                style,
                Vision::Tiles {
                    base: 85.0,
                    tile: 170.0,
                },
            ),
            false,
        );
    };
    (built_geometry(GPT_WIDTH, GPT_HEIGHT, style, vision), true)
}

fn built_geometry(columns: usize, max_height: usize, style: Style, price: Vision) -> Resolved {
    let geometry = Geometry {
        columns,
        max_height,
        style,
        price,
        page_chars: page_chars(columns),
    };
    Resolved {
        standard: geometry,
        history: geometry,
    }
}

fn gemini_profile(model: &str) -> bool {
    if matches!(
        model,
        "gemini"
            | "gemini-pro"
            | "gemini-flash"
            | "gemini-3.1-flash"
            | "gemini-3.1-flash-lite"
            | "gemini-3.6-flash"
            | "gemini-3.7-flash"
    ) {
        return true;
    }
    GEMINI.captures(model).is_some_and(|version| {
        let major = version_number(&version[1]);
        let minor = version
            .get(2)
            .map_or(0, |part| version_number(part.as_str()));
        major > 3 || major == 3 && minor >= 8
    })
}

fn version_number(s: &str) -> u64 {
    s.parse().unwrap_or(u64::MAX)
}

fn overridden(base: Resolved, config: &Value) -> Resolved {
    let pos_int =
        |name: &str, default| positive(&config[name]).map_or(default, |value| value as usize);
    let style = base.standard.style.override_with(&config["style"]);
    let columns = pos_int("stripCols", base.standard.columns);
    let max_height = pos_int("maxHeightPx", base.standard.max_height);
    let high_res = match config["visionTier"].as_str() {
        Some("high-res") => true,
        Some("standard") => false,
        _ => matches!(base.standard.price, Vision::Patch28 { high_res: true }),
    };
    let base_price = match base.standard.price {
        Vision::Patch28 { .. } => Vision::Patch28 { high_res },
        price => price,
    };
    let price = vision(&config["vision"], high_res).unwrap_or(base_price);
    let history_style = if let Some(value) = config.get("historyStyle") {
        Style {
            font: Font::parse(&value["font"]).unwrap_or(style.font),
            ..style
        }
    } else if base.standard.style.row_spacing == 2
        && matches!(base.standard.price, Vision::Patch28 { .. })
    {
        base.history.style
    } else {
        style
    };
    let history_cols = pos_int(
        "historyStripCols",
        if config.get("historyStripCols").is_some() {
            base.history.columns
        } else {
            columns
        },
    );
    let core = Geometry {
        columns,
        max_height,
        style,
        price,
        page_chars: page_chars(columns),
    };
    Resolved {
        standard: core,
        history: Geometry {
            columns: history_cols,
            style: history_style,
            ..core
        },
    }
}

fn page_chars(columns: usize) -> usize {
    columns.saturating_mul(90).min(PAGE_CHARS)
}

fn positive(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| *value > 0.0 && value.is_finite())
}

fn vision(config: &Value, high_res: bool) -> Option<Vision> {
    Some(match config["regime"].as_str()? {
        "patch28" => Vision::Patch28 { high_res },
        "patch" => Vision::Patch32 {
            multiplier: config["multiplier"]
                .as_f64()
                .filter(|value| value.is_finite())?,
            cap: if let Some(cap) = config.get("patchCap") {
                positive(cap)?
            } else {
                f64::INFINITY
            },
        },
        "tile" => Vision::Tiles {
            base: config["base"].as_f64().filter(|value| value.is_finite())?,
            tile: config["perTile"]
                .as_f64()
                .filter(|value| value.is_finite())?,
        },
        "mpix" => Vision::Mpix(positive(&config["tokensPerMegapixel"])?),
        "flat" => Vision::Flat {
            tokens: positive(&config["tokens"])?,
            exact: if let Some(value) = config.get("exact") {
                Some((
                    positive(&value["widthPx"])?,
                    positive(&value["heightPx"])?,
                    positive(&value["tokens"])?,
                ))
            } else {
                None
            },
        },
        _ => return None,
    })
}
