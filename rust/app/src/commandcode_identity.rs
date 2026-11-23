use std::sync::LazyLock;

use icu_normalizer::DecomposingNormalizerBorrowed;
use regex::Regex;
use reqwest::{RequestBuilder, header};

pub(crate) struct Identity {
    pub(crate) directory: String,
    project: String,
}

pub(crate) static IDENTITY: LazyLock<Identity> = LazyLock::new(|| {
    let directory = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let project = slugify(&directory);
    Identity { directory, project }
});

pub(crate) fn client_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header(header::USER_AGENT, "cli")
        .header("x-command-code-version", "1.88.5")
        .header("x-cli-environment", "production")
        .header("x-cli-surface", "cli")
        .header("x-taste-learning", "false")
}

pub(crate) fn session_headers(request: RequestBuilder, session: &str) -> RequestBuilder {
    client_headers(request)
        .header("x-project-slug", IDENTITY.project.as_str())
        .header("x-session-id", session)
}

fn slugify(path: &str) -> String {
    static RULES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
        [
            r"([A-Z]{2,})([0-9]+)",
            r"([a-z0-9]+)([A-Z]{2,})",
            r"([a-z0-9])([A-Z])",
            r"([A-Z]+)([A-Z][a-rt-z0-9]+)",
        ]
        .map(|rule| Regex::new(rule).expect("CommandCode slug rule"))
    });
    let mut text = String::with_capacity(path.len());
    for letter in path.chars() {
        text.push_str(match letter {
            '&' => " and ",
            '🦄' => " unicorn ",
            '♥' => " love ",
            'ß' => "ss",
            'ẞ' => "Ss",
            'ä' => "ae",
            'Ä' => "Ae",
            'ö' => "oe",
            'Ö' => "Oe",
            'ü' => "ue",
            'Ü' => "Ue",
            'Æ' => "AE",
            'æ' => "ae",
            'Ø' => "O",
            'ø' => "o",
            'Œ' => "OE",
            'œ' => "oe",
            'Ð' | 'Đ' => "D",
            'ð' | 'đ' => "d",
            'Þ' => "TH",
            'þ' => "th",
            'Ł' => "L",
            'ł' => "l",
            'ı' => "i",
            letter => {
                text.push(letter);
                continue;
            }
        });
    }
    let mut text: String = DecomposingNormalizerBorrowed::new_nfd()
        .normalize(&text)
        .chars()
        .filter(|ch| !matches!(*ch, '\u{300}'..='\u{36f}'))
        .collect();
    for rule in RULES.iter() {
        text = rule.replace_all(&text, "${1} ${2}").into_owned();
    }
    text.to_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
