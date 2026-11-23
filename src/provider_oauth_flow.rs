use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use super::{antigravity, cline, xai};
use crate::codebuddy_device::Edition as CodeBuddyEdition;
use crate::provider_defaults;
use crate::storage::Storage;

const MAX_ATTEMPTS: usize = 1024;
const ATTEMPT_TTL: Duration = Duration::from_secs(600);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Provider {
    Claude,
    Codex,
    Gemini,
    Antigravity,
    GitHub,
    Cline,
    ClinePass,
    IFlow,
    Xai,
    Kiro,
    GrokCli,
    Trae,
    Nous,
    Kimi,
    MiniMaxCode,
    MiniMaxCodeGlobal,
    CodeBuddy(CodeBuddyEdition),
}

impl Provider {
    pub(super) fn from_id(provider: &str) -> Option<Self> {
        match provider {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini-cli" => Some(Self::Gemini),
            "antigravity" => Some(Self::Antigravity),
            "github" => Some(Self::GitHub),
            "cline" => Some(Self::Cline),
            "clinepass" => Some(Self::ClinePass),
            "iflow" => Some(Self::IFlow),
            "xai" => Some(Self::Xai),
            "kiro" => Some(Self::Kiro),
            "grok-cli" => Some(Self::GrokCli),
            "trae" => Some(Self::Trae),
            "nous" => Some(Self::Nous),
            "kimi" => Some(Self::Kimi),
            "minimax-code" => Some(Self::MiniMaxCode),
            "minimax-code-global" => Some(Self::MiniMaxCodeGlobal),
            "codebuddy-cn" => Some(Self::CodeBuddy(CodeBuddyEdition::China)),
            "codebuddy-intl" => Some(Self::CodeBuddy(CodeBuddyEdition::International)),
            _ => None,
        }
    }

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini-cli",
            Self::Antigravity => "antigravity",
            Self::GitHub => "github",
            Self::Cline => "cline",
            Self::ClinePass => "clinepass",
            Self::IFlow => "iflow",
            Self::Xai => "xai",
            Self::Kiro => "kiro",
            Self::GrokCli => "grok-cli",
            Self::Trae => "trae",
            Self::Nous => "nous",
            Self::Kimi => "kimi",
            Self::MiniMaxCode => "minimax-code",
            Self::MiniMaxCodeGlobal => "minimax-code-global",
            Self::CodeBuddy(edition) => edition.id(),
        }
    }
}

pub(super) enum Operation {
    Authorize,
    DeviceCode,
    Exchange,
    Poll,
}

impl Operation {
    pub(super) fn identify(
        provider: &str,
        method: &Method,
        action: &str,
    ) -> Option<(Provider, Self)> {
        let provider = Provider::from_id(provider)?;
        if matches!(
            provider,
            Provider::Kiro
                | Provider::GrokCli
                | Provider::Nous
                | Provider::Kimi
                | Provider::Trae
                | Provider::MiniMaxCode
                | Provider::MiniMaxCodeGlobal
                | Provider::CodeBuddy(_)
        ) {
            return None;
        }
        let operation = match (provider, method, action) {
            (_, &Method::GET, "authorize") => Self::Authorize,
            (_, &Method::GET, "device-code") => Self::DeviceCode,
            (
                Provider::Claude
                | Provider::Codex
                | Provider::Gemini
                | Provider::Antigravity
                | Provider::Cline
                | Provider::ClinePass
                | Provider::IFlow
                | Provider::Xai,
                &Method::POST,
                "exchange",
            ) => Self::Exchange,
            (Provider::GitHub, &Method::POST, "poll") => Self::Poll,
            _ => return None,
        };
        Some((provider, operation))
    }
}

pub(super) struct ApiError {
    pub(super) status: StatusCode,
    pub(super) message: &'static str,
}

impl ApiError {
    pub(super) fn bad(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }

    pub(super) fn failed(message: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

pub(super) struct Config {
    pub(super) claude: Claude,
    pub(super) codex: Codex,
    pub(super) google: Google,
    pub(super) antigravity: Google,
    antigravity_onboard: String,
    pub(super) github: GitHub,
    pub(super) cline: cline::Config,
    pub(super) clinepass: cline::Config,
    pub(super) iflow: IFlow,
    pub(super) xai: xai::Config,
}

pub(super) struct IFlow {
    pub(super) client: String,
    pub(super) secret: String,
    authorize: String,
    pub(super) token: String,
    pub(super) user: String,
    login_method: String,
    login_type: String,
}

impl IFlow {
    pub(super) fn token_url(&self) -> &str {
        self.fixture_path(&self.token, "token")
    }

    fn fixture_path<'a>(&self, endpoint: &'a str, kind: &str) -> &'a str {
        if endpoint.starts_with("https://iflow.cn/")
            && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            return if kind == "token" {
                "http://127.0.0.1:52989/iflow-fixture/oauth/token"
            } else {
                "http://127.0.0.1:52989/iflow-fixture/api/oauth/getUserInfo"
            };
        }
        endpoint
    }
}

pub(super) struct Claude {
    pub(super) client: String,
    pub(super) authorize: String,
    pub(super) token: String,
    pub(super) scopes: String,
    challenge_method: String,
}

pub(super) struct Codex {
    pub(super) client: String,
    pub(super) authorize: String,
    pub(super) token: String,
    scope: String,
    challenge_method: String,
    fixed_port: u16,
    callback_path: String,
    extra: [(String, String); 3],
}

pub(super) struct Google {
    pub(super) client: String,
    pub(super) secret: String,
    pub(super) authorize: String,
    pub(super) token: String,
    pub(super) user: String,
    pub(super) project: String,
    pub(super) scopes: String,
}

impl Google {
    fn pinned(config: &Value) -> Option<Self> {
        Some(Self {
            client: text(config, "clientId")?,
            secret: text(config, "clientSecret")?,
            authorize: text(config, "authorizeUrl")?,
            token: text(config, "tokenUrl")?,
            user: text(config, "userInfoUrl")?,
            project: "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist".into(),
            scopes: text(config, "scopes")?,
        })
    }
}

pub(super) struct GitHub {
    pub(super) client: String,
    pub(super) device: String,
    pub(super) token: String,
    pub(super) user: String,
    pub(super) copilot: String,
    pub(super) scope: String,
    pub(super) api_version: String,
    pub(super) user_agent: String,
    pub(super) refresh_api_version: String,
    pub(super) refresh_user_agent: String,
    pub(super) vscode_version: String,
    pub(super) chat_version: String,
}

impl Config {
    pub(super) fn pinned() -> Option<Self> {
        let google = provider_defaults::section("gemini-cli.oauth");
        let antigravity = provider_defaults::section("antigravity.oauth");
        let github = provider_defaults::section("github.oauth");
        let github_transport = provider_defaults::section("github.copilot");
        let claude = provider_defaults::section("claude.oauth");
        let codex = provider_defaults::section("codex.oauth");
        let iflow = provider_defaults::section("iflow.oauth");
        Some(Self {
            xai: xai::Config::pinned()?,
            iflow: IFlow {
                client: text(iflow, "clientId")?,
                secret: text(iflow, "clientSecret")?,
                authorize: text(iflow, "authorizeUrl")?,
                token: text(iflow, "tokenUrl")?,
                user: text(iflow, "userInfoUrl")?,
                login_method: text(iflow, "loginMethod")?,
                login_type: text(iflow, "type")?,
            },
            cline: cline::Config::pinned(
                provider_defaults::section("cline.oauth"),
                "tokenExchangeUrl",
            )?,
            clinepass: cline::Config::pinned(
                provider_defaults::section("clinepass.oauth"),
                "tokenUrl",
            )?,
            claude: Claude {
                client: text(claude, "clientId")?,
                authorize: text(claude, "authorizeUrl")?,
                token: text(claude, "tokenUrl")?,
                scopes: text(claude, "scopes")?,
                challenge_method: text(claude, "codeChallengeMethod")?,
            },
            codex: Codex {
                client: text(codex, "clientId")?,
                authorize: text(codex, "authorizeUrl")?,
                token: text(codex, "tokenUrl")?,
                scope: text(codex, "scope")?,
                challenge_method: text(codex, "codeChallengeMethod")?,
                fixed_port: u16::try_from(codex["fixedPort"].as_u64()?).ok()?,
                callback_path: text(codex, "callbackPath")?,
                extra: [
                    (
                        "id_token_add_organizations".into(),
                        text(codex, "id_token_add_organizations")?,
                    ),
                    (
                        "codex_cli_simplified_flow".into(),
                        text(codex, "codex_cli_simplified_flow")?,
                    ),
                    ("originator".into(), text(codex, "originator")?),
                ],
            },
            google: Google::pinned(google)?,
            antigravity: Google {
                project: text(antigravity, "loadCodeAssistEndpoint")?,
                ..Google::pinned(antigravity)?
            },
            antigravity_onboard: text(antigravity, "onboardUserEndpoint")?,
            github: GitHub {
                client: text(github, "clientId")?,
                device: text(github, "deviceCodeUrl")?,
                token: text(github, "tokenUrl")?,
                user: text(github, "userInfoUrl")?,
                copilot: text(github, "copilotTokenUrl")?,
                scope: text(github, "scopes")?,
                api_version: text(github, "apiVersion")?,
                user_agent: text(github, "userAgent")?,
                refresh_api_version: text(github_transport, "apiVersion")?,
                refresh_user_agent: text(github_transport, "userAgent")?,
                vscode_version: text(github_transport, "vscodeVersion")?,
                chat_version: text(github_transport, "chatVersion")?,
            },
        })
    }
}

pub(super) fn text(section: &Value, key: &str) -> Option<String> {
    section[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Clone)]
pub(super) struct OAuth {
    config: Arc<Config>,
    client: Client,
    pending: Arc<Mutex<HashMap<[u8; 32], Attempt>>>,
}

struct Attempt {
    expires: Instant,
    kind: AttemptKind,
}

enum AttemptKind {
    Code {
        provider: Provider,
        redirect: String,
        verifier_hash: [u8; 32],
    },
    Device {
        busy: bool,
    },
}

pub(super) struct Credentials(pub(super) Map<String, Value>);

pub(super) enum Poll {
    Complete(Credentials),
    Status(Value),
}

pub(super) fn device_authorization(redirect: &str) -> Result<Value, ApiError> {
    let verifier = generate_secret(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(digest(&verifier));
    let state = generate_secret(32)?;
    Ok(json!({
        "authUrl":null,"state":state,"codeVerifier":verifier,"codeChallenge":challenge,
        "redirectUri":redirect,"flowType":"device_code","callbackPath":"/callback",
    }))
}

impl OAuth {
    pub(super) fn new(config: Config) -> Result<Self, reqwest::Error> {
        Ok(Self {
            config: Arc::new(config),
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(SERVICE_TIMEOUT)
                .build()?,
            pending: Arc::default(),
        })
    }

    pub(super) async fn authorize(
        &self,
        provider: Provider,
        redirect: &str,
    ) -> Result<Value, ApiError> {
        if provider == Provider::GitHub {
            return device_authorization(redirect);
        }
        let verifier = generate_secret(if provider == Provider::Xai {
            self.config.xai.verifier_bytes
        } else {
            32
        })?;
        let verifier_hash = digest(&verifier);
        let challenge = URL_SAFE_NO_PAD.encode(verifier_hash);
        let state = generate_secret(32)?;
        let (url, flow_type) = match provider {
            Provider::Codex => {
                validate_redirect(redirect)?;
                let config = &self.config.codex;
                let params = [
                    ("response_type", "code"),
                    ("client_id", config.client.as_str()),
                    ("redirect_uri", redirect),
                    ("scope", config.scope.as_str()),
                    ("code_challenge", challenge.as_str()),
                    ("code_challenge_method", config.challenge_method.as_str()),
                ]
                .into_iter()
                .chain(
                    config
                        .extra
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                )
                .chain(std::iter::once(("state", state.as_str())));
                let query = params
                    .map(|(key, value)| {
                        format!("{key}={}", utf8_percent_encode(value, URI_COMPONENT))
                    })
                    .collect::<Vec<_>>()
                    .join("&");
                (
                    json!(format!("{}?{query}", config.authorize)),
                    "authorization_code_pkce",
                )
            }
            Provider::Claude | Provider::Gemini | Provider::Antigravity => {
                validate_redirect(redirect)?;
                let (authorize, client, scope) = match provider {
                    Provider::Claude => {
                        let config = &self.config.claude;
                        (&config.authorize, &config.client, &config.scopes)
                    }
                    Provider::Gemini | Provider::Antigravity => {
                        let config = if provider == Provider::Gemini {
                            &self.config.google
                        } else {
                            &self.config.antigravity
                        };
                        (&config.authorize, &config.client, &config.scopes)
                    }
                    _ => unreachable!(),
                };
                let mut url = Url::parse(authorize)
                    .map_err(|_| ApiError::failed("Invalid authorization endpoint"))?;
                let flow_type = {
                    let mut query = url.query_pairs_mut();
                    if provider == Provider::Claude {
                        query.append_pair("code", "true");
                    }
                    query.extend_pairs([
                        ("client_id", client.as_str()),
                        ("response_type", "code"),
                        ("redirect_uri", redirect),
                        ("scope", scope.as_str()),
                    ]);
                    if provider == Provider::Claude {
                        query
                            .append_pair("code_challenge", &challenge)
                            .append_pair(
                                "code_challenge_method",
                                &self.config.claude.challenge_method,
                            )
                            .append_pair("state", &state);
                        "authorization_code_pkce"
                    } else {
                        query
                            .append_pair("state", &state)
                            .append_pair("access_type", "offline")
                            .append_pair("prompt", "consent");
                        "authorization_code"
                    }
                };
                (json!(url.as_str()), flow_type)
            }
            Provider::IFlow => {
                validate_redirect(redirect)?;
                let config = &self.config.iflow;
                let mut url = Url::parse(&config.authorize)
                    .map_err(|_| ApiError::failed("Invalid authorization endpoint"))?;
                url.query_pairs_mut().extend_pairs([
                    ("loginMethod", config.login_method.as_str()),
                    ("type", config.login_type.as_str()),
                    ("redirect", redirect),
                    ("state", &state),
                    ("client_id", config.client.as_str()),
                ]);
                (json!(url.as_str()), "authorization_code")
            }
            Provider::Xai => {
                validate_redirect(redirect)?;
                let url = self
                    .config
                    .xai
                    .authorize_url(&self.client, redirect, &challenge, &state)
                    .await?;
                (json!(url), "authorization_code_pkce")
            }
            Provider::Cline | Provider::ClinePass => {
                validate_redirect(redirect)?;
                (
                    json!(self.cline_config(provider).authorize(redirect, &state)?),
                    "authorization_code",
                )
            }
            Provider::GitHub
            | Provider::Kiro
            | Provider::GrokCli
            | Provider::Nous
            | Provider::Kimi
            | Provider::Trae
            | Provider::MiniMaxCode
            | Provider::CodeBuddy(_)
            | Provider::MiniMaxCodeGlobal => {
                return Err(ApiError::bad(
                    "Provider does not support authorization code flow",
                ));
            }
        };
        self.register(
            digest(&state),
            Attempt {
                expires: Instant::now() + ATTEMPT_TTL,
                kind: AttemptKind::Code {
                    provider,
                    redirect: redirect.to_owned(),
                    verifier_hash,
                },
            },
        )?;
        let mut result = json!({
            "authUrl": url, "state": state, "codeVerifier": verifier, "codeChallenge": challenge,
            "redirectUri": redirect, "flowType": flow_type, "callbackPath": "/callback"
        });
        if provider == Provider::Codex {
            result["fixedPort"] = json!(self.config.codex.fixed_port);
            result["callbackPath"] = json!(self.config.codex.callback_path);
        }
        if provider == Provider::Xai {
            result["fixedPort"] = json!(self.config.xai.port);
            result["callbackPath"] = json!(self.config.xai.callback_path);
        }
        Ok(result)
    }

    fn register(&self, key: [u8; 32], attempt: Attempt) -> Result<(), ApiError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ApiError::failed("Could not register OAuth attempt"))?;
        let now = Instant::now();
        pending.retain(|_, attempt| attempt.expires > now);
        if pending.len() >= MAX_ATTEMPTS {
            return Err(ApiError {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: "Too many pending OAuth attempts",
            });
        }
        if pending.contains_key(&key) {
            return Err(ApiError::failed("OAuth attempt already registered"));
        }
        pending.insert(key, attempt);
        Ok(())
    }

    pub(super) async fn exchange(
        &self,
        storage: &Storage,
        provider: Provider,
        body: &Value,
    ) -> Result<(Credentials, Option<String>), ApiError> {
        let (Some(code), Some(redirect), Some(verifier)) = (
            nonempty(&body["code"]),
            nonempty(&body["redirectUri"]),
            nonempty(&body["codeVerifier"]),
        ) else {
            return Err(ApiError::bad("Missing required fields"));
        };
        let supplied_state = nonempty(&body["state"]);
        let mut code = code;
        let mut state = supplied_state;
        if provider == Provider::Claude
            && let Some((value, suffix)) = code.split_once('#')
        {
            code = value;
            let suffix = suffix.split('#').next().filter(|suffix| !suffix.is_empty());
            if let (Some(suffix), Some(supplied)) = (suffix, supplied_state)
                && !bool::from(digest(suffix).ct_eq(&digest(supplied)))
            {
                return Err(ApiError::bad("Invalid or expired OAuth state"));
            }
            state = suffix.or(supplied_state);
        }
        let state = state
            .filter(|_| !code.is_empty())
            .ok_or_else(|| ApiError::bad("Invalid or expired OAuth state"))?;
        let key = digest(state);
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| ApiError::failed("Could not verify OAuth state"))?;
            let valid = pending.get(&key).is_some_and(|attempt| {
                attempt.expires > Instant::now()
                    && matches!(&attempt.kind, AttemptKind::Code {
                    provider: expected_provider, redirect: expected, verifier_hash
                } if *expected_provider == provider && expected == redirect
                    && bool::from(verifier_hash.ct_eq(&digest(verifier))))
            });
            if !valid {
                return Err(ApiError::bad("Invalid or expired OAuth state"));
            }
            pending.remove(&key);
        }
        if provider == Provider::Antigravity {
            return antigravity::exchange(storage, &self.config.antigravity, code, redirect).await;
        }
        let credentials = match provider {
            Provider::Gemini => self.exchange_google(code, redirect).await,
            Provider::Claude => self.exchange_claude(code, redirect, verifier, state).await,
            Provider::Codex => self.exchange_codex(code, redirect, verifier).await,
            Provider::IFlow => self.exchange_iflow(code, redirect).await,
            Provider::Xai => {
                self.config
                    .xai
                    .exchange(&self.client, code, redirect, verifier)
                    .await
            }
            Provider::Cline | Provider::ClinePass => {
                self.cline_config(provider)
                    .exchange(&self.client, code, redirect)
                    .await
            }
            Provider::GitHub
            | Provider::Antigravity
            | Provider::Kiro
            | Provider::GrokCli
            | Provider::Nous
            | Provider::Kimi
            | Provider::Trae
            | Provider::MiniMaxCode
            | Provider::CodeBuddy(_)
            | Provider::MiniMaxCodeGlobal => Err(ApiError::bad(
                "Provider does not support authorization code flow",
            )),
        }?;
        Ok((credentials, None))
    }

    pub(super) fn antigravity_onboard(&self) -> &str {
        &self.config.antigravity_onboard
    }

    async fn exchange_iflow(&self, code: &str, redirect: &str) -> Result<Credentials, ApiError> {
        let config = &self.config.iflow;
        let token_url = config.token_url();
        let request = form(
            &self.client,
            token_url,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", config.client.as_str()),
                ("client_secret", config.secret.as_str()),
            ],
        )
        .basic_auth(&config.client, Some(&config.secret));
        let tokens = optional_json(request)
            .await
            .map_err(|_| ApiError::failed("iFlow token exchange failed"))?;
        let access = nonempty(&tokens["access_token"])
            .ok_or_else(|| ApiError::failed("iFlow token exchange failed"))?;
        let user_url = config.fixture_path(&config.user, "user");
        let person = optional_json(
            self.client
                .get(user_url)
                .header(header::ACCEPT, "application/json")
                .query(&[("accessToken", access)]),
        )
        .await
        .ok()
        .filter(|response| response["success"] == true)
        .ok_or_else(|| ApiError::failed("Failed to fetch iFlow user info"))?;
        let user = &person["data"];
        let key = nonempty(&user["apiKey"])
            .map(crate::provider_index::js_trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::failed("Empty API key returned from iFlow"))?;
        let email = [&user["email"], &user["phone"]]
            .into_iter()
            .filter_map(nonempty)
            .map(crate::provider_index::js_trim)
            .find(|value| !value.is_empty())
            .ok_or_else(|| ApiError::failed("Missing account email/phone in user info"))?;
        let mut fields = map_fields(
            &tokens,
            &[
                ("access_token", "accessToken"),
                ("refresh_token", "refreshToken"),
                ("expires_in", "expiresIn"),
            ],
        );
        fields.extend([
            ("apiKey".into(), json!(key)),
            ("email".into(), json!(email)),
        ]);
        if let Some(name) = nonempty(&user["nickname"]).or_else(|| nonempty(&user["name"])) {
            fields.insert("displayName".into(), json!(name));
        }
        Ok(Credentials(fields))
    }

    pub(super) fn has_code_attempt(
        &self,
        provider: Provider,
        state: &str,
        redirect: &str,
        verifier: &str,
    ) -> bool {
        self.pending.lock().ok().is_some_and(|pending| {
            pending.get(&digest(state)).is_some_and(|attempt| {
                attempt.expires > Instant::now()
                    && matches!(&attempt.kind, AttemptKind::Code {
                        provider: expected_provider, redirect: expected_redirect, verifier_hash
                    } if *expected_provider == provider && expected_redirect == redirect
                        && bool::from(verifier_hash.ct_eq(&digest(verifier))))
            })
        })
    }

    fn cline_config(&self, provider: Provider) -> &cline::Config {
        if provider == Provider::ClinePass {
            &self.config.clinepass
        } else {
            &self.config.cline
        }
    }

    async fn exchange_claude(
        &self,
        code: &str,
        redirect: &str,
        verifier: &str,
        state: &str,
    ) -> Result<Credentials, ApiError> {
        let config = &self.config.claude;
        let request = self
            .client
            .post(&config.token)
            .header(header::ACCEPT, "application/json")
            .json(&json!({
                "code": code, "state": state, "grant_type": "authorization_code",
                "client_id": config.client, "redirect_uri": redirect, "code_verifier": verifier
            }));
        let (ok, tokens) = receive_json(request)
            .await
            .map_err(|_| ApiError::failed("Token exchange failed"))?;
        nonempty(&tokens["access_token"])
            .filter(|_| ok)
            .ok_or_else(|| ApiError::failed("Token exchange failed"))?;
        Ok(Credentials(mapped_code_tokens(&tokens)))
    }

    async fn exchange_codex(
        &self,
        code: &str,
        redirect: &str,
        verifier: &str,
    ) -> Result<Credentials, ApiError> {
        let config = &self.config.codex;
        let (ok, tokens) = receive_json(form(
            &self.client,
            &config.token,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", config.client.as_str()),
                ("code", code),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
            ],
        ))
        .await
        .map_err(|_| ApiError::failed("Token exchange failed"))?;
        let access = nonempty(&tokens["access_token"])
            .filter(|_| ok)
            .ok_or_else(|| ApiError::failed("Token exchange failed"))?;
        let identity = token_claims(nonempty(&tokens["id_token"]).unwrap_or_default());
        let chatgpt = &identity["https://api.openai.com/auth"];
        let mut mapped = map_fields(
            &tokens,
            &[
                ("access_token", "accessToken"),
                ("refresh_token", "refreshToken"),
                ("id_token", "idToken"),
                ("expires_in", "expiresIn"),
            ],
        );
        mapped.insert(
            "lastRefreshAt".into(),
            json!(crate::provider_connections::now()),
        );
        let access_identity = token_claims(access);
        if let Some(email) = nonempty(&identity["email"]).or_else(|| {
            ["email", "preferred_username", "sub"]
                .into_iter()
                .find_map(|key| nonempty(&access_identity[key]))
        }) {
            mapped.insert("email".into(), json!(email));
        }
        let mut workspace = Map::new();
        for (key, claim, plain) in [
            ("chatgptAccountId", "chatgpt_account_id", "account_id"),
            ("chatgptPlanType", "chatgpt_plan_type", "plan_type"),
        ] {
            if let Some(data) = nonempty(&chatgpt[claim]).or_else(|| nonempty(&identity[plain])) {
                workspace.insert(key.into(), json!(data));
            }
        }
        if !workspace.is_empty() {
            mapped.insert("providerSpecificData".into(), Value::Object(workspace));
        }
        Ok(Credentials(mapped))
    }

    async fn exchange_google(&self, code: &str, redirect: &str) -> Result<Credentials, ApiError> {
        let config = &self.config.google;
        let (ok, tokens) = receive_json(form(
            &self.client,
            &config.token,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &config.client),
                ("client_secret", &config.secret),
                ("code", code),
                ("redirect_uri", redirect),
            ],
        ))
        .await
        .map_err(|_| ApiError::failed("Token exchange failed"))?;
        let access = nonempty(&tokens["access_token"])
            .filter(|_| ok)
            .ok_or_else(|| ApiError::failed("Token exchange failed"))?;
        let mut profile_url = Url::parse(&config.user)
            .map_err(|_| ApiError::failed("Could not load provider profile"))?;
        profile_url.query_pairs_mut().append_pair("alt", "json");
        let profile = optional_json(self.client.get(profile_url).bearer_auth(access))
            .await
            .map_err(|_| ApiError::failed("Could not load provider profile"))?;
        let project = optional_json(self.client.post(&config.project)
            .bearer_auth(access)
            .json(&json!({"metadata": {"ideType": 9, "platform": platform(), "pluginType": 2}, "mode": 1})))
            .await.unwrap_or_default();
        let project_id = nonempty(&project["cloudaicompanionProject"]["id"])
            .or_else(|| nonempty(&project["cloudaicompanionProject"]))
            .unwrap_or("");
        let mut mapped = mapped_code_tokens(&tokens);
        if let Some(email) = nonempty(&profile["email"]) {
            mapped.insert("email".into(), json!(email));
        }
        mapped.insert("projectId".into(), json!(project_id));
        Ok(Credentials(mapped))
    }

    pub(super) async fn device_code(&self) -> Result<Value, ApiError> {
        let github = &self.config.github;
        let (ok, mut data) = receive_json(form(
            &self.client,
            &github.device,
            &[
                ("client_id", github.client.as_str()),
                ("scope", github.scope.as_str()),
            ],
        ))
        .await
        .map_err(|_| ApiError::failed("Device code request failed"))?;
        let code = nonempty(&data["device_code"])
            .filter(|_| ok)
            .ok_or_else(|| ApiError::failed("Device code request failed"))?;
        let code_key = digest(code);
        let seconds = data["expires_in"]
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .unwrap_or(900)
            .min(3600);
        self.register(
            code_key,
            Attempt {
                expires: Instant::now() + Duration::from_secs(seconds),
                kind: AttemptKind::Device { busy: false },
            },
        )?;
        data["codeVerifier"] = json!(generate_secret(32)?);
        Ok(data)
    }

    pub(super) async fn poll(&self, body: &Value) -> Result<Poll, ApiError> {
        let code =
            nonempty(&body["deviceCode"]).ok_or_else(|| ApiError::bad("Missing device code"))?;
        let key = digest(code);
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| ApiError::failed("Could not verify device code"))?;
            match pending.get_mut(&key) {
                Some(Attempt {
                    expires,
                    kind: AttemptKind::Device { busy },
                }) if *expires > Instant::now() => {
                    if *busy {
                        return Ok(Poll::Status(
                            json!({"success": false, "error": "authorization_pending", "pending": true}),
                        ));
                    }
                    *busy = true;
                }
                _ => {
                    return Ok(Poll::Status(
                        json!({"success": false, "error": "expired_token", "pending": false}),
                    ));
                }
            }
        }
        let mut guard = DevicePoll {
            pending: &self.pending,
            key,
            remove: false,
        };
        let config = &self.config.github;
        let (ok, data) = receive_json(form(
            &self.client,
            &config.token,
            &[
                ("client_id", config.client.as_str()),
                ("device_code", code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        ))
        .await
        .map_err(|_| ApiError::failed("Could not poll the provider for a device token"))?;
        if let Some(access) = nonempty(&data["access_token"]).filter(|_| ok) {
            guard.remove = true;
            let copilot =
                optional_json(github_get(&self.client, config, &config.copilot, access)).await;
            let profile = match &copilot {
                Ok(_) => {
                    optional_json(github_get(&self.client, config, &config.user, access)).await
                }
                Err(_) => Err(()),
            };
            let (Ok(copilot), Ok(profile)) = (copilot, profile) else {
                return Ok(Poll::Status(failed_device_exchange(
                    "Could not load the GitHub account profile",
                )));
            };
            let mut tokens = map_fields(
                &data,
                &[
                    ("access_token", "accessToken"),
                    ("refresh_token", "refreshToken"),
                    ("expires_in", "expiresIn"),
                ],
            );
            if let Some(name) = nonempty(&profile["login"]).or_else(|| nonempty(&profile["name"])) {
                tokens.insert("name".into(), json!(name));
            }
            if let Some(name) = nonempty(&profile["name"]).or_else(|| nonempty(&profile["login"])) {
                tokens.insert("displayName".into(), json!(name));
            }
            tokens.insert(
                "email".into(),
                nonempty(&profile["email"]).map_or(Value::Null, |email| json!(email)),
            );
            let mut extra = map_fields(
                &profile,
                &[
                    ("id", "githubUserId"),
                    ("login", "githubLogin"),
                    ("name", "githubName"),
                    ("email", "githubEmail"),
                ],
            );
            extra.extend(map_fields(
                &copilot,
                &[
                    ("token", "copilotToken"),
                    ("expires_at", "copilotTokenExpiresAt"),
                ],
            ));
            tokens.insert("providerSpecificData".into(), Value::Object(extra));
            return Ok(Poll::Complete(Credentials(tokens)));
        }

        let error = nonempty(&data["error"]).or(ok.then_some("no_access_token"));
        let waiting = matches!(error, Some("authorization_pending" | "slow_down"));
        guard.remove = !waiting;
        let mut status = json!({"success": false, "pending": waiting});
        if let Some(error) = error {
            status["error"] = json!(protocol_error(error));
        }
        if let Some(description) = protocol_description(error, ok, &data) {
            status["errorDescription"] = json!(description);
        }
        Ok(Poll::Status(status))
    }
}

struct DevicePoll<'a> {
    pending: &'a Mutex<HashMap<[u8; 32], Attempt>>,
    key: [u8; 32],
    remove: bool,
}

impl Drop for DevicePoll<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            if self.remove {
                pending.remove(&self.key);
            } else if let Some(Attempt {
                kind: AttemptKind::Device { busy },
                ..
            }) = pending.get_mut(&self.key)
            {
                *busy = false;
            }
        }
    }
}

pub(super) fn failed_device_exchange(message: &'static str) -> Value {
    json!({"success": false, "error": "exchange_failed", "errorDescription": message, "fatal": true, "pending": false})
}

fn validate_redirect(uri: &str) -> Result<(), ApiError> {
    if uri.len() > 2048 {
        return Err(ApiError::bad("Invalid redirect URI"));
    }
    let url = Url::parse(uri).map_err(|_| ApiError::bad("Invalid redirect URI"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ApiError::bad("Invalid redirect URI"));
    }
    Ok(())
}

pub(super) fn generate_secret(length: usize) -> Result<String, ApiError> {
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::failed("Could not generate OAuth state"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

pub(super) fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

pub(super) fn form(client: &Client, url: &str, values: &[(&str, &str)]) -> RequestBuilder {
    client
        .post(url)
        .header(header::ACCEPT, "application/json")
        .form(values)
}

pub(super) async fn receive_json(request: RequestBuilder) -> Result<(bool, Value), ()> {
    let response = request.send().await.map_err(|_| ())?;
    let success = response.status().is_success();
    let json = response.json::<Value>().await.map_err(|_| ())?;
    Ok((success, json))
}

async fn optional_json(request: RequestBuilder) -> Result<Value, ()> {
    let response = request.send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Ok(Value::Null);
    }
    response.json::<Value>().await.map_err(|_| ())
}

fn github_get(client: &Client, config: &GitHub, url: &str, token: &str) -> RequestBuilder {
    client
        .get(url)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/json")
        .header("x-github-api-version", &config.api_version)
        .header(header::USER_AGENT, &config.user_agent)
}

pub(crate) fn token_claims(token: &str) -> Value {
    let mut segments = token.split('.');
    segments.next();
    let Some(payload) = segments
        .next()
        .filter(|_| segments.next().is_some() && segments.next().is_none())
    else {
        return Value::Null;
    };
    URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_default()
}

pub(super) fn mapped_code_tokens(tokens: &Value) -> Map<String, Value> {
    map_fields(
        tokens,
        &[
            ("access_token", "accessToken"),
            ("refresh_token", "refreshToken"),
            ("expires_in", "expiresIn"),
            ("scope", "scope"),
        ],
    )
}

fn map_fields(source: &Value, names: &[(&str, &str)]) -> Map<String, Value> {
    names
        .iter()
        .filter_map(|(from, to)| {
            source
                .get(*from)
                .map(|value| ((*to).to_owned(), value.clone()))
        })
        .collect()
}

fn protocol_error(error: &str) -> &'static str {
    match error {
        "authorization_pending" => "authorization_pending",
        "slow_down" => "slow_down",
        "expired_token" => "expired_token",
        "access_denied" => "access_denied",
        "incorrect_device_code" => "incorrect_device_code",
        "unsupported_grant_type" => "unsupported_grant_type",
        "invalid_grant" => "invalid_grant",
        "no_access_token" => "no_access_token",
        _ => "device_authorization_failed",
    }
}

fn protocol_description(error: Option<&str>, ok: bool, response: &Value) -> Option<&'static str> {
    if error == Some("no_access_token") {
        return Some("No access token received");
    }
    if response["error_description"].as_str().is_none()
        && (!ok || response["message"].as_str().is_none())
    {
        return None;
    }
    Some(match error {
        Some("authorization_pending") => "Authorization is pending",
        Some("slow_down") => "Wait before polling again",
        Some("expired_token") => "The device code has expired",
        Some("access_denied") => "Authorization was denied",
        _ => "Device authorization failed",
    })
}

pub(super) fn platform() -> u8 {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    }
}
