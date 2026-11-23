use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use url::Url;

use crate::bedrock_transport::{Credentials, Redactions, region, sign};

use super::{
    SpeechInput, SpeechState, SynthesisError, audio_bytes, fixture, pcm_to_wav, speech_request,
};

const SPEECH_PATH: &str = "/v1/speech";
const FIXTURE: &str = "http://127.0.0.1:52989/aws-polly/v1/speech";
const ENGINES: &[&str] = &["standard", "neural", "long-form", "generative"];
const MAX_AUDIO: usize = 128 * 1024 * 1024;

pub(super) async fn synthesize(
    state: &SpeechState,
    settings: &Value,
    account: &Value,
    model: &str,
    speech: &SpeechInput<'_>,
) -> Result<Response, SynthesisError> {
    let invalid = |message: &str| SynthesisError {
        status: StatusCode::BAD_REQUEST,
        message: message.to_owned(),
        protected: false,
    };
    let unauthorized = || SynthesisError {
        status: StatusCode::UNAUTHORIZED,
        message: "AWS Polly requires valid AWS credentials".to_owned(),
        protected: false,
    };
    if !matches!(account["authType"].as_str(), Some("apikey" | "api_key")) {
        return Err(unauthorized());
    }
    let credentials = Credentials::from_account(account).map_err(|_| unauthorized())?;
    let region = region(account).map_err(invalid)?;
    let endpoint = Url::parse(&format!(
        "https://polly.{region}.amazonaws.com{SPEECH_PATH}"
    ))
    .map_err(|_| invalid("Invalid AWS Polly endpoint"))?;
    let (engine, voice) = match model.split_once('/') {
        Some((engine, voice))
            if ENGINES.contains(&engine) && !voice.is_empty() && !voice.contains('/') =>
        {
            (engine, voice)
        }
        Some(_) => return Err(invalid("Use an AWS Polly engine followed by a voice ID")),
        None if model.is_empty() => ("standard", "Joanna"),
        None if ENGINES.contains(&model) => (model, "Joanna"),
        None => ("standard", model),
    };
    let (output, format) = match speech.response_format {
        "mp3" | "json" => ("mp3", "mp3"),
        "wav" => ("pcm", "wav"),
        "pcm" => ("pcm", "pcm"),
        "ogg" | "ogg_vorbis" => ("ogg_vorbis", "ogg"),
        "opus" | "ogg_opus" => ("ogg_opus", "ogg"),
        "mulaw" => ("mulaw", "mulaw"),
        "alaw" => ("alaw", "alaw"),
        _ => return Err(invalid("Unsupported AWS Polly audio format")),
    };
    let mut body =
        json!({"Engine":engine,"VoiceId":voice,"Text":speech.text,"OutputFormat":output});
    if !speech.language.is_empty() {
        body["LanguageCode"] = json!(speech.language);
    }
    if output == "pcm" {
        body["SampleRate"] = json!("16000");
    }
    let body = body.to_string();
    let headers = sign(
        &endpoint,
        "POST",
        &region,
        "polly",
        BTreeMap::from([("content-type", "application/json")]),
        body.as_bytes(),
        credentials,
    )
    .map_err(|_| unauthorized())?;
    let direct = if fixture() {
        FIXTURE
    } else {
        endpoint.as_str()
    };
    let routed = speech_request(state, settings, account, endpoint.as_str(), direct, false).await?;
    let protected = routed.protected;
    let request = if protected {
        routed.request
    } else {
        state.protected.post(direct)
    };
    let redactions = Redactions::for_credentials(credentials);
    let fail = |message: &str| {
        SynthesisError::upstream(redactions.redact(message)).protect(protected, None, None)
    };
    let mut response = request
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| fail("Could not reach AWS Polly"))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(fail("AWS Polly returned an unexpected redirect"));
    }
    let mut audio = Vec::new();
    let limit = if status.is_success() {
        MAX_AUDIO
    } else {
        64 * 1024
    };
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| fail("AWS Polly audio response was interrupted"))?
    {
        if chunk.len() > limit - audio.len() {
            return Err(fail("AWS Polly response is too large"));
        }
        audio.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let detail = serde_json::from_slice::<Value>(&audio)
            .ok()
            .and_then(|value| {
                value["message"]
                    .as_str()
                    .or_else(|| value["Message"].as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("AWS Polly TTS failed: {}", status.as_u16()));
        return Err(fail(&detail));
    }
    if audio.is_empty() {
        return Err(fail("AWS Polly returned no audio"));
    }
    let audio = if format == "wav" {
        pcm_to_wav(audio, 16_000)?
    } else {
        Bytes::from(audio)
    };
    audio_bytes(audio, format, speech.response_format)
}
