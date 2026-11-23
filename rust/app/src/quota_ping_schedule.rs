use std::sync::OnceLock;

use jiff::{
    Timestamp,
    civil::Date,
    tz::{Offset, TimeZone},
};
use serde_json::Value;

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: f64 = 3_600_000.0;
const GRACE_MS: f64 = 1_800_000.0;

static SYSTEM_ZONE: OnceLock<TimeZone> = OnceLock::new();

pub(crate) struct Schedule {
    anchor: (i8, i8),
    timezone: TimeZone,
    window_hours: f64,
    slots_per_day: usize,
}

impl Schedule {
    pub(crate) fn from_value(raw: &Value) -> Option<Self> {
        if raw["enabled"] != true {
            return None;
        }
        let window_hours = raw
            .get("windowHours")
            .and_then(number)
            .unwrap_or(5.0)
            .clamp(0.5, 24.0);
        let max_slots = (24.0 / window_hours).floor().max(1.0);
        let slots_per_day = raw
            .get("slotsPerDay")
            .and_then(number)
            .unwrap_or(3.0)
            .clamp(1.0, max_slots)
            .round() as usize;
        let anchor = anchor_text(&raw["anchor"])
            .and_then(parse_anchor)
            .unwrap_or((9, 30));
        let timezone = raw["timezone"]
            .as_str()
            .and_then(parse_timezone)
            .unwrap_or_else(|| SYSTEM_ZONE.get_or_init(TimeZone::system).clone());
        Some(Self {
            anchor,
            timezone,
            window_hours,
            slots_per_day,
        })
    }

    pub(crate) fn due_slot(&self, now_ms: i64) -> Option<String> {
        let now = now_ms as f64;
        let (_, date, index) = [-DAY_MS, 0, DAY_MS]
            .into_iter()
            .filter_map(|day| {
                let sample = Timestamp::from_millisecond(now_ms.checked_add(day)?).ok()?;
                let date = self.timezone.to_datetime(sample).date();
                Some((date, self.anchor_timestamp(date)? as f64))
            })
            .flat_map(|(date, first_at)| {
                (0..self.slots_per_day).map(move |index| {
                    (
                        first_at + index as f64 * self.window_hours * HOUR_MS,
                        date,
                        index,
                    )
                })
            })
            .filter(|(at, _, _)| *at <= now && now - at <= GRACE_MS)
            .max_by(|(left, _, _), (right, _, _)| left.total_cmp(right))?;
        Some(format!(
            "{}-{:02}-{:02}#{index}",
            date.year(),
            date.month(),
            date.day()
        ))
    }

    fn anchor_timestamp(&self, date: Date) -> Option<i64> {
        let naive = TimeZone::UTC
            .to_timestamp(date.at(self.anchor.0, self.anchor.1, 0, 0))
            .ok()?;
        let naive_ms = naive.as_millisecond();
        let initial_offset_ms = i64::from(self.timezone.to_offset(naive).seconds()) * 1000;
        let candidate = Timestamp::from_millisecond(naive_ms - initial_offset_ms).ok()?;
        let refined_offset_ms = i64::from(self.timezone.to_offset(candidate).seconds()) * 1000;
        Some(naive_ms - refined_offset_ms)
    }
}

fn anchor_text(raw: &Value) -> Option<&str> {
    match raw {
        Value::String(text) => Some(text),
        Value::Array(entries) if entries.len() == 1 => anchor_text(&entries[0]),
        _ => None,
    }
}

fn parse_anchor(text: &str) -> Option<(i8, i8)> {
    let (hour, minute) = trim(text).split_once(':')?;
    if !(1..=2).contains(&hour.len()) || minute.len() != 2 {
        return None;
    }
    let hour = decimal(hour.as_bytes())?;
    let minute = decimal(minute.as_bytes())?;
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

fn parse_timezone(zone: &str) -> Option<TimeZone> {
    if matches!(zone.as_bytes().first(), Some(b'+' | b'-')) {
        return fixed_timezone(zone.as_bytes());
    }
    if ["", "localtime", "posixrules", "Factory", "Etc/Unknown"]
        .iter()
        .any(|invalid| zone.eq_ignore_ascii_case(invalid))
    {
        return None;
    }
    TimeZone::get(zone).ok()
}

fn fixed_timezone(zone: &[u8]) -> Option<TimeZone> {
    let (hour, minute) = match zone {
        [_, a, b] => (decimal(&[*a, *b])?, 0),
        [_, a, b, c, d] | [_, a, b, b':', c, d] => (decimal(&[*a, *b])?, decimal(&[*c, *d])?),
        _ => return None,
    };
    if hour > 23 || minute > 59 {
        return None;
    }
    let seconds = i32::from(hour) * 3600 + i32::from(minute) * 60;
    let signed = if zone[0] == b'-' { -seconds } else { seconds };
    Offset::from_seconds(signed).ok().map(TimeZone::fixed)
}

fn decimal(bytes: &[u8]) -> Option<i8> {
    bytes.iter().try_fold(0, |answer, digit| {
        digit
            .is_ascii_digit()
            .then(|| answer * 10 + (digit - b'0') as i8)
    })
}

fn number(raw: &Value) -> Option<f64> {
    match raw {
        Value::Bool(true) => Some(1.0),
        Value::Null | Value::Bool(false) => Some(0.0),
        Value::String(text) => parse_number(text),
        Value::Array(entries) => match entries.as_slice() {
            [] | [Value::Null] => Some(0.0),
            [item @ (Value::Array(_) | Value::String(_) | Value::Number(_))] => number(item),
            _ => None,
        },
        _ => raw.as_f64(),
    }
}

fn parse_number(text: &str) -> Option<f64> {
    let text = trim(text);
    if text.is_empty() {
        return Some(0.0);
    }
    let number = match text.as_bytes() {
        [b'0', b'x' | b'X', ..] => radix_number(&text[2..], 16)?,
        [b'0', b'o' | b'O', ..] => radix_number(&text[2..], 8)?,
        [b'0', b'b' | b'B', ..] => radix_number(&text[2..], 2)?,
        _ => text.parse::<f64>().ok()?,
    };
    number.is_finite().then_some(number)
}

fn radix_number(text: &str, radix: u32) -> Option<f64> {
    if text.is_empty() {
        return None;
    }
    text.chars().try_fold(0.0, |value, digit| {
        Some(value * f64::from(radix) + f64::from(digit.to_digit(radix)?))
    })
}

fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}
