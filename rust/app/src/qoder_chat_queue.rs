use std::collections::VecDeque;
use std::time::Duration;

use serde_json::Value;

use crate::provider_index::js_trim;

const DEFAULT_WAIT: Duration = Duration::from_secs(600);
const NESTED: &[&str] = &[
    "error", "data", "body", "message", "cause", "result", "payload",
];

#[derive(Default)]
pub(super) struct Info {
    code: Option<String>,
    subcode: Option<String>,
    pub(super) is_queued: Option<bool>,
    pub(super) model_key: Option<String>,
    pub(super) queue_type: Option<String>,
    available: Option<bool>,
    retry_after: Option<f64>,
    wait: Option<f64>,
}

pub(super) fn budget() -> Duration {
    std::env::var("QODER_MODEL_QUEUE_MAX_WAIT_MS")
        .ok()
        .and_then(|raw| js_trim(&raw).parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .and_then(|value| Duration::try_from_secs_f64(value / 1000.0).ok())
        .unwrap_or(DEFAULT_WAIT)
}

impl Info {
    pub(super) fn queued(body: &str) -> Option<Self> {
        let mut todo = VecDeque::from([parse(body)?]);
        let mut info = Self::default();
        for _ in 0..128 {
            let Some(next) = todo.pop_front() else { break };
            match next {
                Value::Object(mut object) => {
                    info.merge(&object);
                    for key in NESTED {
                        match object.shift_remove(*key) {
                            Some(node @ (Value::Object(_) | Value::Array(_))) => {
                                todo.push_back(node)
                            }
                            Some(Value::String(raw)) => {
                                if let Some(node) = parse(&raw) {
                                    todo.push_back(node);
                                }
                            }
                            _ => (),
                        }
                    }
                }
                Value::Array(rows) => todo.extend(
                    rows.into_iter()
                        .filter(|node| node.is_object() || node.is_array()),
                ),
                _ => (),
            }
        }
        (info.is_queued == Some(true)
            || info.available == Some(false)
            || info.subcode.as_deref() == Some("RESOURCE_LIMIT")
            || info.code.as_deref() == Some("10605"))
        .then_some(info)
    }

    pub(super) fn status(body: &str) -> Option<Self> {
        let mut info = Self::default();
        info.merge(parse(body)?.as_object()?);
        info.is_queued.is_some().then_some(info)
    }

    fn merge(&mut self, object: &serde_json::Map<String, Value>) {
        let text = |key| {
            object
                .get(key)?
                .as_str()
                .map(js_trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let flag = |key| {
            let value = object.get(key)?;
            match value.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => value.as_bool(),
            }
        };
        let number = |key| {
            let value = object.get(key)?;
            let parsed = if let Some(raw) = value.as_str() {
                let raw = js_trim(raw);
                let digits = raw.strip_prefix('-').unwrap_or(raw);
                let mut parts = digits.split('.');
                let first = parts.next()?;
                if first.is_empty()
                    || !first.bytes().all(|ch| ch.is_ascii_digit())
                    || parts.next().is_some_and(|fraction| {
                        fraction.is_empty() || !fraction.bytes().all(|ch| ch.is_ascii_digit())
                    })
                    || parts.next().is_some()
                {
                    return None;
                }
                raw.parse::<f64>().ok()?
            } else {
                value.as_f64()?
            };
            parsed.is_finite().then_some(parsed)
        };
        self.code = self.code.take().or_else(|| text("code"));
        self.subcode = self.subcode.take().or_else(|| text("subCode"));
        self.is_queued = self.is_queued.or_else(|| flag("isQueued"));
        self.model_key = self.model_key.take().or_else(|| text("modelKey"));
        self.queue_type = self.queue_type.take().or_else(|| text("queueType"));
        self.available = self.available.or_else(|| flag("serviceAvailable"));
        self.retry_after = self.retry_after.or_else(|| number("retryAfterSeconds"));
        self.wait = self.wait.or_else(|| number("waitTime"));
    }

    pub(super) fn delay(&self, status: bool) -> Duration {
        let floor = if status { 30.0 } else { 15.0 };
        Duration::from_secs_f64(
            self.retry_after
                .or(self.wait)
                .unwrap_or(30.0)
                .clamp(floor, 60.0),
        )
    }
}

fn parse(body: &str) -> Option<Value> {
    let text = js_trim(body);
    if !text.starts_with(['{', '[']) {
        return None;
    }
    serde_json::from_str(text).ok()
}
