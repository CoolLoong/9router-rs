use std::time::Duration;

use axum::body::Bytes;
use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use reqwest::{RequestBuilder, Response, header};
use serde_json::{Value, json};
use url::Url;

use crate::sse_reader::{ReadLine, SseReader};

const FIXTURE: &str = "http://127.0.0.1:52989";
const OPENROUTER_MODEL: &str = "openai/gpt-4o-mini-tts";
const OPENROUTER_AUDIO_LIMIT: usize = 128 * 1024 * 1024;
const MIMO_MODEL: &str = "mimo-v2.5-tts";
const MIMO_VOICE: &str = "mimo_default";
const FISH_AUDIO_MODEL: &str = "s2.1-pro-free";
const NVIDIA_MODELS: &[&str] = &["fastpitch", "tacotron2"];
const COQUI_MODELS: &[&str] = &["tts_models/en/ljspeech/tacotron2-DDC"];
const TORTOISE_MODELS: &[&str] = &["tortoise-v2"];
const PLAYHT_MODELS: &[&str] = &["PlayDialog", "Play3.0-mini"];
const PLAYHT_VOICE: &str =
    "s3://voice-cloning-zero-shot/d9ff78ba-d016-47f6-b0ef-dd630f59414e/female-cs/manifest.json";
const MINIMAX_MODELS: &[&str] = &[
    "speech-2.8-hd",
    "speech-2.8-turbo",
    "speech-2.6-hd",
    "speech-2.6-turbo",
    "speech-02-hd",
    "speech-02-turbo",
    "speech-01-hd",
    "speech-01-turbo",
];
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Cartesia,
    Coqui,
    ElevenLabs,
    Deepgram,
    FishAudio,
    Inworld,
    Minimax,
    MinimaxCn,
    Nvidia,
    OpenRouter,
    PlayHt,
    Tortoise,
}

pub struct Audio {
    pub bytes: Bytes,
    pub format: String,
    pub base64: Option<String>,
}

impl Provider {
    pub fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "cartesia" => Some(Self::Cartesia),
            "coqui" => Some(Self::Coqui),
            "elevenlabs" => Some(Self::ElevenLabs),
            "deepgram" if deepgram_endpoint().is_some() => Some(Self::Deepgram),
            "fish-audio" => Some(Self::FishAudio),
            "inworld" => Some(Self::Inworld),
            "minimax" => Some(Self::Minimax),
            "minimax-cn" => Some(Self::MinimaxCn),
            "nvidia" => Some(Self::Nvidia),
            "openrouter" => Some(Self::OpenRouter),
            "playht" => Some(Self::PlayHt),
            "tortoise" => Some(Self::Tortoise),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cartesia => "cartesia",
            Self::Coqui => "coqui",
            Self::ElevenLabs => "ElevenLabs",
            Self::Deepgram => "deepgram",
            Self::FishAudio => "fish-audio",
            Self::Inworld => "inworld",
            Self::Minimax => "minimax",
            Self::MinimaxCn => "minimax-cn",
            Self::Nvidia => "NVIDIA",
            Self::OpenRouter => "OpenRouter",
            Self::PlayHt => "playht",
            Self::Tortoise => "tortoise",
        }
    }

    pub(super) fn local(self) -> bool {
        matches!(self, Self::Coqui | Self::Tortoise)
    }

    pub(super) fn supports_proxy(self) -> bool {
        matches!(
            self,
            Self::Cartesia
                | Self::Coqui
                | Self::ElevenLabs
                | Self::FishAudio
                | Self::Inworld
                | Self::Minimax
                | Self::MinimaxCn
                | Self::Nvidia
                | Self::OpenRouter
                | Self::PlayHt
                | Self::Tortoise
        )
    }

    pub(super) fn allows_relay(self, model: &str) -> bool {
        if self == Self::ElevenLabs {
            let (_, voice) = elevenlabs_model_voice(model);
            return !voice.is_empty()
                && voice
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte));
        }
        self.supports_proxy() && !self.local()
    }

    pub(super) fn endpoint(self, model: &str, fixture: bool) -> Result<Url, String> {
        let (endpoint, local_path) = match self {
            Self::Cartesia => ("https://api.cartesia.ai/tts/bytes", "/cartesia/tts/bytes"),
            Self::Coqui => ("http://localhost:5002/api/tts", "/coqui/api/tts"),
            Self::ElevenLabs => (
                "https://api.elevenlabs.io/v1/text-to-speech",
                "/elevenlabs/v1/text-to-speech",
            ),
            Self::Deepgram => (
                deepgram_endpoint().ok_or("Provider does not support speech synthesis")?,
                "/deepgram/v1/speak",
            ),
            Self::FishAudio => ("https://api.fish.audio/v1/tts", "/fish-audio/v1/tts"),
            Self::Inworld => (
                "https://api.inworld.ai/tts/v1/voice",
                "/inworld/tts/v1/voice",
            ),
            Self::Minimax => ("https://api.minimax.io/v1/t2a_v2", "/minimax/v1/t2a_v2"),
            Self::MinimaxCn => (
                "https://api.minimaxi.com/v1/t2a_v2",
                "/minimax-cn/v1/t2a_v2",
            ),
            Self::Nvidia => (
                "https://integrate.api.nvidia.com/v1/audio/speech",
                "/speech-nvidia/v1/audio/speech",
            ),
            Self::OpenRouter => (
                "https://openrouter.ai/api/v1/chat/completions",
                "/speech-openrouter/api/v1/chat/completions",
            ),
            Self::PlayHt => (
                "https://api.play.ht/api/v2/tts/stream",
                "/playht/api/v2/tts/stream",
            ),
            Self::Tortoise => ("http://localhost:5000/api/tts", "/tortoise/api/tts"),
        };
        if fixture {
            request_url(self, &format!("{FIXTURE}{local_path}"), model)
        } else {
            request_url(self, endpoint, model)
        }
    }
}

fn deepgram_endpoint() -> Option<&'static str> {
    let registry = include_str!("../../../open-sse/providers/registry/deepgram.js");
    let tts = registry.split_once("ttsConfig:")?.1;
    Some(
        tts.split_once("baseUrl:")?
            .1
            .trim_start()
            .strip_prefix('"')?
            .split_once('"')?
            .0,
    )
}

pub(super) async fn synthesize_mimo(
    request: RequestBuilder,
    text: &str,
    model: &str,
    api_key: Option<&str>,
    language: &str,
    style: &str,
    protected: bool,
) -> Result<Audio, String> {
    let key = api_key.ok_or("xiaomi-mimo API key required")?;
    let (model, voice) = if model.rfind('/').is_some_and(|index| index > 0) {
        parse_model_voice(model, &[MIMO_MODEL])
    } else {
        (MIMO_MODEL, "")
    };
    let mut instructions = Vec::new();
    if !language.is_empty() {
        instructions.push(format!("Speak in {language}."));
    }
    if !style.is_empty() {
        instructions.push(style.to_owned());
    }
    let mut messages = Vec::new();
    if !instructions.is_empty() {
        messages.push(json!({"role":"user","content":instructions.join(" ")}));
    }
    messages.push(json!({"role":"assistant","content":text}));
    let response = request
        .bearer_auth(key)
        .json(&json!({
            "model":model,"stream":false,"messages":messages,
            "audio":{"format":"wav","voice":or(voice, MIMO_VOICE)},
        }))
        .send()
        .await
        .map_err(|_| "fetch failed")?;
    let status = response.status();
    if protected && status.is_redirection() {
        return Err("Speech upstream redirect refused".into());
    }
    let raw = response.text().await.map_err(|_| "terminated")?;
    let body: Value = serde_json::from_str(&raw).unwrap_or_default();
    let upstream = nonempty(&body["error"]["message"]);
    if !status.is_success() {
        let message = upstream
            .or_else(|| (!raw.is_empty()).then_some(raw.as_str()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("MiMo TTS error ({})", status.as_u16()));
        return Err(message.replace(key, "[redacted]"));
    }
    let audio = &body["choices"][0]["message"]["audio"];
    let base64 = nonempty(&audio["data"]).ok_or_else(|| {
        upstream
            .unwrap_or("MiMo TTS returned no audio")
            .replace(key, "[redacted]")
    })?;
    let format = nonempty(&audio["format"]).unwrap_or("wav");
    Ok(Audio {
        bytes: Bytes::from(decode_base64(base64)),
        format: format.into(),
        base64: Some(base64.into()),
    })
}

fn request_url(provider: Provider, endpoint: &str, model: &str) -> Result<Url, String> {
    let mut url = Url::parse(endpoint).map_err(|_| "Invalid speech provider endpoint")?;
    match provider {
        Provider::ElevenLabs => {
            let (_, voice) = elevenlabs_model_voice(model);
            url.path_segments_mut()
                .map_err(|_| "Invalid speech provider endpoint")?
                .push(voice);
        }
        Provider::Deepgram => {
            let (model, _) = parse_model_voice(model, &[]);
            url.query_pairs_mut()
                .append_pair("model", or(model, "aura-asteria-en"));
        }
        _ => {}
    }
    Ok(url)
}

fn elevenlabs_model_voice(model: &str) -> (&str, &str) {
    let mut parts = model.split('/');
    let first = parts.next().unwrap_or_default();
    parts
        .next()
        .map_or(("eleven_flash_v2_5", first), |voice| (first, voice))
}

pub(super) async fn synthesize(
    request: RequestBuilder,
    provider: Provider,
    text: &str,
    model: &str,
    api_key: Option<&str>,
    protected: bool,
) -> Result<Audio, String> {
    if provider.local() {
        let body = if provider == Provider::Coqui {
            let (_, voice) = parse_model_voice(model, COQUI_MODELS);
            let mut body = json!({"text":text});
            if !voice.is_empty() {
                body["speaker_id"] = json!(voice);
            }
            body
        } else {
            let (_, voice) = parse_model_voice(model, TORTOISE_MODELS);
            json!({"text":text,"voice":or(voice,"random")})
        };
        let response = submit(request.json(&body), protected).await?;
        return binary_audio(response, "wav").await;
    }
    let key = api_key.filter(|value| !value.is_empty()).ok_or_else(|| {
        if provider == Provider::OpenRouter {
            "No OpenRouter API key configured".into()
        } else {
            format!("{} API key required", provider.name())
        }
    })?;
    let mut request = if provider == Provider::OpenRouter {
        request
            .header("HTTP-Referer", "https://endpoint-proxy.local")
            .header("X-Title", "Endpoint Proxy")
    } else {
        request
    };
    let mut secret = key;
    let (header, authorization, body) = match provider {
        Provider::Cartesia => {
            let (model, voice) = parse_model_voice(model, &[]);
            request = request.header("Cartesia-Version", "2024-06-10");
            let mut body = json!({
                "model_id":or(model,"sonic-2"),"transcript":text,
                "output_format":{"container":"mp3","bit_rate":128000,"sample_rate":44100},
            });
            if !voice.is_empty() {
                body["voice"] = json!({"mode":"id","id":voice});
            }
            ("x-api-key", key.to_owned(), body)
        }
        Provider::ElevenLabs => {
            let (model, _) = elevenlabs_model_voice(model);
            (
                "xi-api-key",
                key.to_owned(),
                json!({
                    "text":text,"model_id":model,"voice_settings":{"stability":0.5,"similarity_boost":0.75},
                }),
            )
        }
        Provider::Deepgram => (
            "authorization",
            format!("Token {key}"),
            json!({"text":text}),
        ),
        Provider::FishAudio => {
            let (model, voice) = parse_model_voice(model, &[]);
            request = request.header("model", or(model, FISH_AUDIO_MODEL));
            let mut body = json!({"text":text,"format":"mp3"});
            if !voice.is_empty() {
                body["reference_id"] = json!(voice);
            }
            ("authorization", format!("Bearer {key}"), body)
        }
        Provider::Inworld => {
            let (model, voice) = parse_model_voice(model, &[]);
            (
                "authorization",
                format!("Basic {key}"),
                json!({
                    "text":text,"voiceId":or(voice,"Alex"),"modelId":or(model,"inworld-tts-1.5-mini"),
                    "audioConfig":{"audioEncoding":"MP3"},
                }),
            )
        }
        Provider::Minimax | Provider::MinimaxCn => {
            let (model, voice) = parse_model_voice(model, MINIMAX_MODELS);
            (
                "authorization",
                format!("Bearer {key}"),
                json!({
                    "model":or(model,"speech-2.8-hd"),"text":text,"stream":false,
                    "language_boost":"auto","output_format":"hex",
                    "voice_setting":{"voice_id":or(voice,"English_expressive_narrator"),"speed":1,"vol":1,"pitch":0},
                    "audio_setting":{"sample_rate":32000,"bitrate":128000,"format":"mp3","channel":1},
                }),
            )
        }
        Provider::Nvidia => {
            let (model, voice) = parse_model_voice(model, NVIDIA_MODELS);
            (
                "authorization",
                format!("Bearer {key}"),
                json!({"input":{"text":text},"voice":or(voice,"default"),"model":model}),
            )
        }
        Provider::OpenRouter => {
            let (model, voice) = model
                .rsplit_once('/')
                .filter(|(upstream, _)| upstream.contains('/'))
                .unwrap_or((OPENROUTER_MODEL, or(model, "alloy")));
            (
                "authorization",
                format!("Bearer {key}"),
                json!({
                    "model":model,"modalities":["text","audio"],"audio":{"voice":voice,"format":"wav"},
                    "stream":true,"messages":[{"role":"user","content":text}],
                }),
            )
        }
        Provider::PlayHt => {
            let mut fields = key.split(':');
            let user = fields.next().unwrap_or_default();
            secret = fields
                .next()
                .filter(|token| !token.is_empty())
                .unwrap_or(key);
            let mut user_header =
                header::HeaderValue::from_str(user).map_err(|_| "Invalid PlayHT user ID")?;
            user_header.set_sensitive(true);
            request = request
                .header("X-USER-ID", user_header)
                .header(header::ACCEPT, "audio/mpeg");
            let (model, voice) = parse_model_voice(model, PLAYHT_MODELS);
            (
                "authorization",
                format!("Bearer {secret}"),
                json!({
                    "text":text,"voice":or(voice,PLAYHT_VOICE),"voice_engine":or(model,PLAYHT_MODELS[0]),
                    "output_format":"mp3","speed":1,
                }),
            )
        }
        Provider::Coqui | Provider::Tortoise => {
            unreachable!("local speech uses the unauthenticated transport")
        }
    };
    let mut value = header::HeaderValue::from_str(&authorization)
        .map_err(|_| "Invalid speech provider credential")?;
    value.set_sensitive(true);
    let result = submit(request.header(header, value).json(&body), protected).await?;
    let audio = match provider {
        Provider::ElevenLabs => elevenlabs(result).await,
        Provider::Cartesia | Provider::Deepgram | Provider::FishAudio | Provider::PlayHt => {
            binary_audio(result, "mp3").await
        }
        Provider::Nvidia => binary_audio(result, "wav").await,
        Provider::Inworld => inworld(result).await,
        Provider::Minimax | Provider::MinimaxCn => minimax(result).await,
        Provider::OpenRouter => openrouter(result).await,
        Provider::Coqui | Provider::Tortoise => unreachable!("local speech decoded above"),
    };
    audio.map_err(|message| {
        message
            .replace(key, "[redacted]")
            .replace(secret, "[redacted]")
    })
}

async fn submit(request: RequestBuilder, protected: bool) -> Result<Response, String> {
    let response = request.send().await.map_err(|_| "fetch failed")?;
    if protected && response.status().is_redirection() {
        return Err("Speech upstream redirect refused".into());
    }
    Ok(response)
}

async fn openrouter(response: Response) -> Result<Audio, String> {
    let status = response.status();
    if !status.is_success() {
        let body = response.json::<Value>().await.unwrap_or_default();
        return Err(nonempty(&body["error"]["message"])
            .map(str::to_owned)
            .unwrap_or_else(|| format!("OpenRouter TTS failed: {}", status.as_u16())));
    }
    let mut reader = SseReader::new(response).with_line_limit(OPENROUTER_AUDIO_LIMIT);
    let mut base64 = String::new();
    loop {
        match reader.next(Duration::from_secs(60), None).await {
            ReadLine::Line(line, true) => {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(value) = serde_json::from_str::<Value>(data)
                    && let Some(audio) = nonempty(&value["choices"][0]["delta"]["audio"]["data"])
                {
                    if audio.len() > OPENROUTER_AUDIO_LIMIT.saturating_sub(base64.len()) {
                        return Err("OpenRouter TTS audio is too large".into());
                    }
                    base64.push_str(audio);
                }
            }
            ReadLine::Line(_, false) | ReadLine::End => break,
            ReadLine::Disconnected => return Err("terminated".into()),
            ReadLine::Timeout => return Err("OpenRouter TTS stream timed out".into()),
        }
    }
    if base64.is_empty() {
        return Err("OpenRouter TTS returned no audio data".into());
    }
    Ok(Audio {
        bytes: Bytes::from(decode_base64(&base64)),
        format: "wav".into(),
        base64: Some(base64),
    })
}

fn parse_model_voice<'a>(model: &'a str, known: &[&'static str]) -> (&'a str, &'a str) {
    let default = known.first().copied().unwrap_or_default();
    if model.is_empty() {
        return (default, "");
    }
    let exact = known
        .iter()
        .filter_map(|id| {
            if model == *id {
                return Some((*id, ""));
            }
            let voice = model.strip_prefix(id)?.strip_prefix('/')?;
            Some((*id, voice))
        })
        .max_by_key(|(id, _)| id.len());
    if let Some(result) = exact {
        return result;
    }
    match model.rsplit_once('/') {
        Some((left, right)) if !left.is_empty() => (left, right),
        _ => (or(default, model), model),
    }
}

async fn elevenlabs(response: Response) -> Result<Audio, String> {
    let status = response.status();
    if !status.is_success() {
        let result = response.json::<Value>().await.unwrap_or_default();
        return Err(nonempty(&result["detail"]["message"])
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ElevenLabs TTS failed: {}", status.as_u16())));
    }
    let bytes = response.bytes().await.map_err(|_| "terminated")?;
    if bytes.len() < 1024 {
        return Err("ElevenLabs TTS returned empty audio".into());
    }
    Ok(Audio {
        bytes,
        format: "mp3".into(),
        base64: None,
    })
}

async fn binary_audio(response: Response, default_format: &str) -> Result<Audio, String> {
    let response = successful(response).await?;
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let format = if mime.contains("wav") {
        "wav"
    } else if mime.contains("mpeg") || mime.contains("mp3") {
        "mp3"
    } else if mime.contains("ogg") {
        "ogg"
    } else if mime.contains("pcm") {
        "pcm"
    } else {
        default_format
    };
    let bytes = response.bytes().await.map_err(|_| "terminated")?;
    if bytes.len() < 100 {
        return Err("Upstream returned empty audio".into());
    }
    Ok(Audio {
        bytes,
        format: format.into(),
        base64: None,
    })
}

async fn inworld(response: Response) -> Result<Audio, String> {
    let response = successful(response).await?;
    let data = response
        .json::<Value>()
        .await
        .map_err(|_| "Invalid JSON from Inworld TTS")?;
    let base64 = nonempty(&data["audioContent"]).ok_or("Inworld TTS returned no audio")?;
    Ok(Audio {
        bytes: Bytes::from(decode_base64(base64)),
        format: "mp3".into(),
        base64: Some(base64.to_owned()),
    })
}

async fn minimax(response: Response) -> Result<Audio, String> {
    let status = response.status();
    let raw = response.text().await.map_err(|_| "terminated")?;
    let data: Value = serde_json::from_str(&raw).unwrap_or_default();
    let result = if data["base_resp"].is_object() {
        &data["base_resp"]
    } else {
        &data["baseResp"]
    };
    let code = result
        .get("status_code")
        .filter(|value| !value.is_null())
        .or_else(|| result.get("statusCode").filter(|value| !value.is_null()));
    let message = [
        &result["status_msg"],
        &result["statusMsg"],
        &data["message"],
    ]
    .into_iter()
    .find_map(nonempty);
    if !status.is_success() {
        return Err(message
            .or_else(|| (!raw.is_empty()).then_some(raw.as_str()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("MiniMax TTS error ({})", status.as_u16())));
    }
    if code.is_some_and(|value| json_number(value) != Some(0.0)) {
        return Err(message.unwrap_or("MiniMax TTS upstream error").to_owned());
    }
    let hex = nonempty(&data["data"]["audio"])
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or("MiniMax TTS returned no audio")?;
    if hex.len() % 2 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("MiniMax TTS returned invalid audio".into());
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().as_chunks::<2>().0 {
        bytes.push((hex_digit(pair[0]) << 4) | hex_digit(pair[1]));
    }
    let format = nonempty(&data["extra_info"]["audio_format"])
        .or_else(|| nonempty(&data["extraInfo"]["audioFormat"]))
        .unwrap_or("mp3");
    Ok(Audio {
        bytes: Bytes::from(bytes),
        format: format.into(),
        base64: None,
    })
}

fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'A'..=b'F' => byte - b'A' + 10,
        _ => byte - b'a' + 10,
    }
}

fn json_number(input: &Value) -> Option<f64> {
    input
        .as_f64()
        .or_else(|| input.as_bool().map(|flag| f64::from(u8::from(flag))))
        .or_else(|| {
            let text = input.as_str()?.trim();
            if text.is_empty() {
                Some(0.0)
            } else {
                text.parse().ok()
            }
        })
}

async fn successful(response: Response) -> Result<Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or_default();
    let message = [
        &value["error"]["message"],
        &value["message"],
        &value["detail"]["message"],
        &value["detail"],
    ]
    .into_iter()
    .find_map(nonempty)
    .or_else(|| (!text.is_empty()).then_some(text.as_str()));
    Err(message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Upstream error ({})", status.as_u16())))
}

fn decode_base64(text: &str) -> Vec<u8> {
    let mut normalized: Vec<_> = text
        .bytes()
        .take_while(|byte| *byte != b'=')
        .filter_map(|byte| match byte {
            b'-' => Some(b'+'),
            b'_' => Some(b'/'),
            b'+' | b'/' => Some(byte),
            byte if byte.is_ascii_alphanumeric() => Some(byte),
            _ => None,
        })
        .collect();
    if normalized.len() % 4 == 1 {
        normalized.pop();
    }
    BASE64.decode(normalized).unwrap_or_default()
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}
