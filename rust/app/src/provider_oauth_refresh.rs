use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Client, Method, header};
use rusqlite::TransactionBehavior;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use super::{
    RefreshError, cline,
    flow::{Config, Provider, nonempty},
    grok, kimi, kimi_portal, kiro, minimax_code, nous, nous_portal,
    store::expiry,
    trae,
};
use crate::provider_http::{self, Delivery, http_client};
use crate::{provider_connections as connections, storage::Storage};

const DEFAULT_LEAD_MS: i64 = 5 * 60 * 1000;
const CLAUDE_LEAD_MS: i64 = 4 * 60 * 60 * 1000;
const CODEX_LEAD_MS: i64 = 10 * 60 * 1000;
const MINIMAX_CODE_LEAD_MS: i64 = 10 * 60 * 1000;
const CODEX_MAX_AGE_MS: i64 = 8 * 24 * 60 * 60 * 1000;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const CLINE_FIXTURE: &str = "http://127.0.0.1:52989/cline/api/v1/auth/refresh";
const GITHUB_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/github/oauth/access_token";
const COPILOT_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/github/copilot_internal/v2/token";
const CLAUDE_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/oauth-probes/claude/token";
const CODEX_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/oauth-probes/codex/token";
const GEMINI_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/oauth-probes/gemini-cli/token";
const ANTIGRAVITY_TOKEN_FIXTURE: &str = "http://127.0.0.1:52989/oauth-probes/antigravity/token";
const CREDENTIAL_FIELDS: &[&str] = &[
    "id",
    "provider",
    "authType",
    "accessToken",
    "refreshToken",
    "idToken",
    "expiresAt",
    "tokenExpiresAt",
    "expiresIn",
    "lastRefreshAt",
    "lastRefresh",
];
const COPILOT_FIELDS: &[&str] = &["copilotToken", "copilotTokenExpiresAt"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    Proactive,
    Background,
    Rejected,
    KiroProtected,
    KiroTest { protected: bool },
    GrokProtected,
    Routed(bool),
    ChatRouted { rejected: bool },
    QuotaRouted { rejected: bool },
    RelayRouted([u8; 32]),
    CopilotOnly { relay: Option<[u8; 32]> },
    GitHubRouted { rejected: bool },
    GitHubRelay { rejected: bool, identity: [u8; 32] },
}

impl RefreshMode {
    pub(crate) fn github(rejected: bool, relay: Option<[u8; 32]>) -> Self {
        if let Some(identity) = relay {
            Self::GitHubRelay { rejected, identity }
        } else {
            Self::GitHubRouted { rejected }
        }
    }
}

enum AccessEgress {
    Vendor(Option<bool>),
    Bound(Option<[u8; 32]>),
}

#[derive(Clone)]
pub(super) struct RefreshService {
    config: Arc<Config>,
    client: Client,
    nous: nous_portal::Portal,
    kimi: kimi_portal::Portal,
    locks: Arc<Mutex<AccountLocks>>,
}

type AccountLocks = HashMap<(PathBuf, String), Weak<AccountRefresh>>;

#[derive(Default)]
struct AccountRefresh {
    completed: AtomicU64,
    last_failure: AsyncMutex<Option<FailedRefresh>>,
}

struct FailedRefresh {
    request: (RefreshMode, [u8; 32]),
    generation: u64,
    error: RefreshError,
}

static SERVICE: LazyLock<RefreshService> = LazyLock::new(|| {
    let config = Config::pinned().expect("pinned provider OAuth configuration");
    RefreshService::new(config).expect("provider refresh HTTP client")
});

pub async fn refresh_account(
    storage: &Storage,
    observed: &Value,
    mode: RefreshMode,
) -> Result<Value, RefreshError> {
    SERVICE.request(storage, observed, mode).await
}

pub(crate) fn refresh_due_for_provider(provider: &str, account: &Value) -> bool {
    Provider::from_id(provider)
        .is_some_and(|provider| should_refresh(provider, account, Utc::now().timestamp_millis()))
}

pub(crate) fn test_token_url(provider: &str) -> Option<&'static str> {
    connection_test_endpoint(&SERVICE.config, Provider::from_id(provider)?)
}

pub(crate) async fn refresh_xai_routed(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<Value, RefreshError> {
    if account["provider"] != "xai" || account["authType"] != "oauth" {
        return Err(RefreshError::Unsupported);
    }
    let endpoint = SERVICE.config.xai.token_endpoint();
    let route = Delivery::select(storage, settings, account, endpoint)
        .await
        .map_err(|_| RefreshError::Unsupported)?;
    refresh_account(storage, account, RefreshMode::Routed(route.protected())).await
}

pub(crate) async fn refresh_xai_inference(
    storage: &Storage,
    account: &Value,
    destination: &str,
    expected: &crate::proxy_transport::Selection,
) -> Result<Value, RefreshError> {
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    if !matches!(crate::provider_http::select(storage, &settings, account, destination).await, Ok(route) if &route == expected)
    {
        return Err(RefreshError::Unsupported);
    }
    let updated = refresh_xai_routed(storage, &settings, account).await?;
    if updated["isActive"] == false || updated["accessToken"].as_str().is_none_or(str::is_empty) {
        return Err(RefreshError::ReauthenticationRequired);
    }
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    if !matches!(crate::provider_http::select(storage, &settings, &updated, destination).await, Ok(route) if &route == expected)
    {
        return Err(RefreshError::Unsupported);
    }
    Ok(updated)
}

fn connection_test_endpoint(config: &Config, provider: Provider) -> Option<&str> {
    match provider {
        Provider::Claude => Some(&config.claude.token),
        Provider::Codex => Some(&config.codex.token),
        Provider::Gemini => Some(&config.google.token),
        Provider::Antigravity => Some(&config.antigravity.token),
        Provider::Cline => Some(&config.cline.refresh),
        Provider::ClinePass => Some(&config.clinepass.refresh),
        Provider::CodeBuddy(edition) => Some(edition.refresh_url()),
        Provider::Xai => Some(config.xai.token_endpoint()),
        _ => None,
    }
}

fn authenticated_binding(
    binding: Option<provider_http::Bound>,
) -> Result<provider_http::Bound, RefreshError> {
    binding
        .filter(|binding| {
            !matches!(binding.route, crate::proxy_transport::Route::Relay { .. })
                || binding.relay_secret().is_some()
        })
        .ok_or(RefreshError::Unsupported)
}

pub(crate) fn nous_refresh_due(account: &Value) -> bool {
    nous::refresh_due(
        account,
        Utc::now().timestamp_millis(),
        refresh_lead(Provider::Nous),
    )
}

pub(super) fn kimi_due(account: &Value) -> bool {
    should_refresh(Provider::Kimi, account, Utc::now().timestamp_millis())
}

pub(super) fn kiro_due(account: &Value) -> bool {
    should_refresh(Provider::Kiro, account, Utc::now().timestamp_millis())
}

pub(crate) fn background_candidate(account: &Value, now_ms: i64) -> bool {
    let Some(provider) = account["provider"].as_str().and_then(Provider::from_id) else {
        return false;
    };
    if provider == Provider::GrokCli
        && !nonempty(&account["refreshToken"])
            .is_some_and(|token| account[grok::ATTEMPT] != grok::marker(token))
    {
        return false;
    }
    let lead = refresh_lead(provider).max(30 * 60 * 1000);
    if provider == Provider::Nous
        && !nonempty(&account["refreshToken"]).is_some_and(|token| {
            account[nous::ATTEMPT] != nous::marker(token)
                && nous::refresh_due(account, now_ms, lead)
        })
    {
        return false;
    }
    if provider == Provider::Kiro
        && !kiro::Grant::from_account(account)
            .or_else(|| kiro::Grant::for_relay(account))
            .is_some_and(|grant| account[kiro::ATTEMPT] != kiro::marker(grant.token))
    {
        return false;
    }
    if provider == Provider::Kimi
        && !kimi::Grant::from_account(account)
            .is_some_and(|grant| account[kimi::ATTEMPT] != kimi::marker(grant.token))
    {
        return false;
    }
    if minimax_code::Region::from_provider(provider).is_some()
        && !nonempty(&account["refreshToken"])
            .is_some_and(|token| account[minimax_code::ATTEMPT] != minimax_code::marker(token))
    {
        return false;
    }
    account["isActive"] == true
        && account["authType"] == "oauth"
        && nonempty(&account["refreshToken"]).is_some()
        && (provider == Provider::Nous
            || time_ms(or_nonnull(
                &account["expiresAt"],
                &account["tokenExpiresAt"],
            ))
            .is_some_and(|expiry| expiry.saturating_sub(now_ms) < lead))
}

impl RefreshService {
    pub(super) fn new(config: Config) -> Result<Self, reqwest::Error> {
        Ok(Self {
            config: Arc::new(config),
            client: Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            nous: nous_portal::Portal::new()?,
            kimi: kimi_portal::Portal::new()?,
            locks: Arc::default(),
        })
    }

    pub(super) async fn request(
        &self,
        storage: &Storage,
        observed: &Value,
        mode: RefreshMode,
    ) -> Result<Value, RefreshError> {
        let id = nonempty(&observed["id"]).ok_or(RefreshError::AccountUnavailable)?;
        let provider = nonempty(&observed["provider"])
            .and_then(Provider::from_id)
            .filter(|_| observed["authType"] == "oauth")
            .ok_or(RefreshError::Unsupported)?;
        let account = self.account_lock(storage, id)?;
        let joined_after = account.completed.load(Ordering::Acquire);
        let service = self.clone();
        let storage = storage.clone();
        let observed = observed.clone();
        let id = id.to_owned();

        tokio::spawn(async move {
            let mut last_failure = account.last_failure.lock().await;
            let request = (mode, fingerprint(&observed));
            let pending_error = last_failure
                .as_ref()
                .filter(|result| result.request == request && result.generation > joined_after)
                .map(|result| result.error);
            let outcome = service
                .refresh(&storage, &id, provider, &observed, mode, pending_error)
                .await;
            if pending_error.is_none() {
                let generation = account.completed.fetch_add(1, Ordering::Release) + 1;
                *last_failure = outcome
                    .as_ref()
                    .err()
                    .copied()
                    .filter(|error| {
                        matches!(
                            error,
                            RefreshError::ProviderUnavailable
                                | RefreshError::ReauthenticationRequired
                        )
                    })
                    .map(|error| FailedRefresh {
                        request,
                        generation,
                        error,
                    });
            }
            outcome
        })
        .await
        .map_err(|_| RefreshError::ProviderUnavailable)?
    }

    fn account_lock(
        &self,
        storage: &Storage,
        id: &str,
    ) -> Result<Arc<AccountRefresh>, RefreshError> {
        let key = (storage.root().to_path_buf(), id.to_owned());
        let mut locks = self.locks.lock().map_err(|_| RefreshError::Storage)?;
        locks.retain(|_, existing| existing.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(AccountRefresh::default());
        locks.insert(key, Arc::downgrade(&lock));
        Ok(lock)
    }

    async fn refresh(
        &self,
        storage: &Storage,
        id: &str,
        provider: Provider,
        observed: &Value,
        mode: RefreshMode,
        pending_error: Option<RefreshError>,
    ) -> Result<Value, RefreshError> {
        let mut latest = load(storage, id, provider).await?;
        if !same_credentials(observed, &latest) {
            return Ok(latest);
        }
        if let Some(error) = pending_error {
            return Err(error);
        }
        let background = mode == RefreshMode::Background;
        if matches!(provider, Provider::Cline | Provider::ClinePass)
            && !background
            && !matches!(mode, RefreshMode::Routed(_) | RefreshMode::RelayRouted(_))
            && !(provider == Provider::ClinePass && matches!(mode, RefreshMode::QuotaRouted { .. }))
        {
            let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
            if !matches!(
                crate::cline_transport::egress(storage, &settings, &latest).await,
                Ok(None)
            ) {
                return Err(RefreshError::Unsupported);
            }
        }

        if let RefreshMode::CopilotOnly { relay } = mode {
            if provider != Provider::GitHub {
                return Err(RefreshError::Unsupported);
            }
            let access =
                nonempty(&latest["accessToken"]).ok_or(RefreshError::ReauthenticationRequired)?;
            return match self
                .refresh_copilot(access, Some((storage, &latest)), relay)
                .await
            {
                Ok(patch) => apply_if_current(storage, latest, patch).await,
                Err(error) => {
                    let current = load(storage, id, provider).await?;
                    if same_credentials(&latest, &current) {
                        Err(error)
                    } else {
                        Ok(current)
                    }
                }
            };
        }
        let test = match mode {
            RefreshMode::Routed(protected) => {
                if provider != Provider::Nous {
                    connection_test_endpoint(&self.config, provider)
                        .ok_or(RefreshError::Unsupported)?;
                }
                Some(protected)
            }
            _ => None,
        };
        let github_routed = matches!(
            mode,
            RefreshMode::GitHubRouted { .. } | RefreshMode::GitHubRelay { .. }
        );
        let github_relay = match mode {
            RefreshMode::GitHubRelay { identity, .. } => Some(identity),
            _ => None,
        };
        if github_routed && provider != Provider::GitHub {
            return Err(RefreshError::Unsupported);
        }
        let chat_routed = matches!(mode, RefreshMode::ChatRouted { .. });
        if chat_routed
            && !matches!(
                provider,
                Provider::Codex | Provider::Gemini | Provider::IFlow
            )
        {
            return Err(RefreshError::Unsupported);
        }
        let quota_routed = matches!(mode, RefreshMode::QuotaRouted { .. });
        let relay = match mode {
            RefreshMode::RelayRouted(identity)
                if matches!(
                    provider,
                    Provider::Kiro
                        | Provider::GrokCli
                        | Provider::Cline
                        | Provider::ClinePass
                        | Provider::Codex
                        | Provider::Claude
                        | Provider::Gemini
                        | Provider::Antigravity
                ) =>
            {
                Some(identity)
            }
            RefreshMode::RelayRouted(_) => return Err(RefreshError::Unsupported),
            _ => None,
        };
        if quota_routed
            && !matches!(
                provider,
                Provider::Codex
                    | Provider::Claude
                    | Provider::Gemini
                    | Provider::CodeBuddy(_)
                    | Provider::Kimi
                    | Provider::ClinePass
                    | Provider::IFlow
            )
        {
            return Err(RefreshError::Unsupported);
        }
        let force = matches!(
            mode,
            RefreshMode::Rejected
                | RefreshMode::GitHubRouted { rejected: true }
                | RefreshMode::GitHubRelay { rejected: true, .. }
                | RefreshMode::ChatRouted { rejected: true }
                | RefreshMode::QuotaRouted { rejected: true }
                | RefreshMode::RelayRouted(_)
        ) || test.is_some();
        if provider == Provider::GitHub && force {
            return self
                .github_rejected(storage, id, latest, github_routed, github_relay)
                .await;
        }
        let now_ms = Utc::now().timestamp_millis();
        if background && !background_candidate(&latest, now_ms) {
            return Ok(latest);
        }
        if provider == Provider::Kiro {
            if background {
                let (protected, relay) = kiro::background_route(storage, &latest).await?;
                return self
                    .refresh_kiro(storage, latest, protected, false, relay)
                    .await;
            }
            let test = matches!(mode, RefreshMode::KiroTest { .. });
            let protected = relay.is_some()
                || matches!(
                    mode,
                    RefreshMode::KiroProtected | RefreshMode::KiroTest { protected: true }
                );
            if force || protected || should_refresh(provider, &latest, now_ms) {
                return self
                    .refresh_kiro(storage, latest, protected, test, relay)
                    .await;
            }
            return Ok(latest);
        }
        if provider == Provider::Trae {
            if force || background || should_refresh(provider, &latest, now_ms) {
                let patch = trae::refresh(storage, &latest).await?;
                return apply_if_current(storage, latest, patch).await;
            }
            return Ok(latest);
        }
        if provider == Provider::GrokCli {
            if background {
                let (protected, relay) = grok::background_route(storage, &latest).await?;
                return self.refresh_grok(storage, latest, protected, relay).await;
            }
            return match mode {
                RefreshMode::Rejected
                | RefreshMode::GrokProtected
                | RefreshMode::RelayRouted(_) => {
                    self.refresh_grok(storage, latest, mode != RefreshMode::Rejected, relay)
                        .await
                }
                _ => Ok(latest),
            };
        }
        if provider == Provider::Nous {
            return if force
                || background
                || nous::refresh_due(&latest, now_ms, refresh_lead(provider))
            {
                self.refresh_nous(storage, latest, test).await
            } else {
                Ok(latest)
            };
        }
        if provider == Provider::Kimi {
            return if force || background || should_refresh(provider, &latest, now_ms) {
                self.refresh_kimi(storage, latest).await
            } else {
                Ok(latest)
            };
        }
        if let Some(region) = minimax_code::Region::from_provider(provider) {
            return if force || background || should_refresh(provider, &latest, now_ms) {
                self.refresh_minimax_code(storage, latest, provider, region)
                    .await
            } else {
                Ok(latest)
            };
        }
        let mut access_failure = None;
        if force || background || should_refresh(provider, &latest, now_ms) {
            if let Some(refresh) = nonempty(&latest["refreshToken"]) {
                match self
                    .refresh_access(
                        provider,
                        refresh,
                        if quota_routed
                            || github_routed
                            || relay.is_some()
                            || chat_routed && provider == Provider::IFlow
                            || background && provider != Provider::Xai
                        {
                            Some((
                                storage,
                                &latest,
                                AccessEgress::Bound(github_relay.or(relay)),
                            ))
                        } else {
                            test.map(|protected| {
                                (storage, &latest, AccessEgress::Vendor(Some(protected)))
                            })
                            .or_else(|| {
                                (matches!(provider, Provider::Antigravity | Provider::CodeBuddy(_))
                                    || background
                                    || chat_routed)
                                    .then_some((storage, &latest, AccessEgress::Vendor(None)))
                            })
                        },
                    )
                    .await
                {
                    Ok(patch) => latest = apply_if_current(storage, latest, patch).await?,
                    Err(error) => {
                        let current = load(storage, id, provider).await?;
                        if !same_credentials(&latest, &current) {
                            return Ok(current);
                        }
                        access_failure = Some(error);
                    }
                }
            } else if force && provider != Provider::GitHub {
                return Err(RefreshError::ReauthenticationRequired);
            }
        }

        if provider == Provider::GitHub && (force || copilot_due(&latest, now_ms)) {
            let access = nonempty(&latest["accessToken"])
                .ok_or(access_failure.unwrap_or(RefreshError::ReauthenticationRequired))?;
            match self
                .refresh_copilot(
                    access,
                    (github_routed || background).then_some((storage, &latest)),
                    github_relay,
                )
                .await
            {
                Ok(patch) => return apply_if_current(storage, latest, patch).await,
                Err(error) => {
                    let current = load(storage, id, provider).await?;
                    if !same_credentials(&latest, &current) {
                        return Ok(current);
                    }
                    return Err(access_failure.unwrap_or(error));
                }
            }
        }
        access_failure.map_or(Ok(latest), Err)
    }

    async fn github_rejected(
        &self,
        storage: &Storage,
        id: &str,
        account: Value,
        routed: bool,
        relay: Option<[u8; 32]>,
    ) -> Result<Value, RefreshError> {
        if let Some(access) = nonempty(&account["accessToken"])
            && let Ok(patch) = self
                .refresh_copilot(access, routed.then_some((storage, &account)), relay)
                .await
        {
            return apply_if_current(storage, account, patch).await;
        }
        let github = Provider::GitHub;
        let current = load(storage, id, github).await?;
        if !same_credentials(&account, &current) {
            return Ok(current);
        }
        let Some(refresh) = nonempty(&current["refreshToken"]) else {
            return Err(RefreshError::ProviderUnavailable);
        };
        let patch = match self
            .refresh_access(
                github,
                refresh,
                routed.then_some((storage, &current, AccessEgress::Bound(relay))),
            )
            .await
        {
            Ok(patch) => patch,
            Err(error) => {
                let latest = load(storage, id, github).await?;
                return if same_credentials(&current, &latest) {
                    Err(error)
                } else {
                    Ok(latest)
                };
            }
        };
        let obtained_access = patch["accessToken"].clone();
        let current = apply_if_current(storage, current, patch).await?;
        if current["accessToken"] == obtained_access
            && let Some(access) = nonempty(&current["accessToken"])
            && let Ok(patch) = self
                .refresh_copilot(access, routed.then_some((storage, &current)), relay)
                .await
        {
            return apply_if_current(storage, current, patch).await;
        }
        Ok(current)
    }

    async fn refresh_kiro(
        &self,
        storage: &Storage,
        account: Value,
        protected: bool,
        test: bool,
        relay: Option<[u8; 32]>,
    ) -> Result<Value, RefreshError> {
        let grant = if test {
            kiro::Grant::for_test(&account, protected)
        } else if relay.is_some() {
            kiro::Grant::for_relay(&account)
        } else {
            kiro::Grant::for_route(&account, protected)
        }
        .ok_or(RefreshError::Unsupported)?;
        let marker = kiro::marker(grant.token);
        if account[kiro::ATTEMPT] == marker {
            return Err(RefreshError::ReauthenticationRequired);
        }
        let request = grant.request(storage, &account, protected, relay).await?;
        let reserved =
            match reserve_rotation(storage, account.clone(), Provider::Kiro, marker).await? {
                Reservation::Reserved(current) => current,
                Reservation::Changed(current) => return Ok(current),
                Reservation::Blocked => return Err(RefreshError::ReauthenticationRequired),
            };
        match grant.execute(request).await {
            kiro::Outcome::Complete(patch) => {
                let access = patch["accessToken"].clone();
                let refreshed_at = patch["lastRefreshAt"].clone();
                let current = apply_if_current(storage, reserved, patch).await?;
                if current["accessToken"] == access
                    && current["lastRefreshAt"] == refreshed_at
                    && let Some(profile) = grant
                        .discover_profile(storage, &current, protected, relay)
                        .await
                {
                    return apply_if_current(storage, current, profile).await;
                }
                Ok(current)
            }
            kiro::Outcome::Throttled => {
                apply_if_current(
                    storage,
                    reserved,
                    Map::from_iter([(kiro::ATTEMPT.into(), Value::Null)]),
                )
                .await?;
                Err(RefreshError::ProviderUnavailable)
            }
            kiro::Outcome::Uncertain => Err(RefreshError::ReauthenticationRequired),
        }
    }

    async fn refresh_grok(
        &self,
        storage: &Storage,
        account: Value,
        protected: bool,
        relay: Option<[u8; 32]>,
    ) -> Result<Value, RefreshError> {
        let refresh =
            nonempty(&account["refreshToken"]).ok_or(RefreshError::ReauthenticationRequired)?;
        let marker = grok::marker(refresh);
        if account[grok::ATTEMPT] == marker {
            return Err(RefreshError::ReauthenticationRequired);
        }
        let request = grok::prepare(storage, &account, refresh, protected, relay).await?;
        let reserved = match reserve_rotation(storage, account, Provider::GrokCli, marker).await? {
            Reservation::Reserved(current) => current,
            Reservation::Changed(current) => return Ok(current),
            Reservation::Blocked => return Err(RefreshError::ReauthenticationRequired),
        };
        match grok::execute(request).await {
            Ok(patch) => apply_if_current(storage, reserved, patch).await,
            Err(error) => {
                let id = nonempty(&reserved["id"]).ok_or(RefreshError::AccountUnavailable)?;
                let current = load(storage, id, Provider::GrokCli).await?;
                if same_credentials(&reserved, &current) {
                    Err(error)
                } else {
                    Ok(current)
                }
            }
        }
    }

    async fn refresh_nous(
        &self,
        storage: &Storage,
        account: Value,
        expected: Option<bool>,
    ) -> Result<Value, RefreshError> {
        let token =
            nonempty(&account["refreshToken"]).ok_or(RefreshError::ReauthenticationRequired)?;
        let marker = nous::check_marker(&account, token)?;
        let request = self
            .nous
            .prepare(
                storage,
                &account,
                nous_portal::Endpoint::Refresh(token),
                expected,
            )
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        let reserved = match reserve_rotation(storage, account, Provider::Nous, marker).await? {
            Reservation::Reserved(current) => current,
            Reservation::Changed(current) => return Ok(current),
            Reservation::Blocked => return Err(RefreshError::ReauthenticationRequired),
        };
        match nous::execute(request).await {
            nous::Outcome::Complete(patch) => {
                let access = patch["accessToken"].clone();
                let refreshed_at = patch["lastRefreshAt"].clone();
                let current = apply_if_current(storage, reserved, patch).await?;
                if current["accessToken"] != access || current["lastRefreshAt"] != refreshed_at {
                    return Ok(current);
                }
                let Some(access) = nonempty(&current["accessToken"]) else {
                    return Ok(current);
                };
                let minted = self.nous.mint(storage, &current, access, expected).await;
                if minted.is_null() {
                    return Ok(current);
                }
                let (key, details) = nous_portal::credentials(access, &minted);
                let patch = Map::from_iter([
                    ("apiKey".into(), json!(key)),
                    ("providerSpecificData".into(), Value::Object(details)),
                ]);
                apply_if_current(storage, current, patch).await
            }
            nous::Outcome::Throttled => {
                apply_if_current(
                    storage,
                    reserved,
                    Map::from_iter([(nous::ATTEMPT.into(), Value::Null)]),
                )
                .await?;
                Err(RefreshError::ProviderUnavailable)
            }
            nous::Outcome::Uncertain => Err(RefreshError::ReauthenticationRequired),
        }
    }

    async fn refresh_kimi(&self, storage: &Storage, account: Value) -> Result<Value, RefreshError> {
        let grant = kimi::Grant::from_account(&account).ok_or(RefreshError::Unsupported)?;
        let marker = kimi::marker(grant.token);
        if account[kimi::ATTEMPT] == marker {
            return Err(RefreshError::ReauthenticationRequired);
        }
        let request = self
            .kimi
            .prepare(
                storage,
                &account,
                grant.device,
                kimi_portal::Endpoint::Refresh {
                    token: grant.token,
                    client_id: grant.client_id,
                },
            )
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        let reserved = match reserve_rotation(storage, account, Provider::Kimi, marker).await? {
            Reservation::Reserved(current) => current,
            Reservation::Changed(current) => return Ok(current),
            Reservation::Blocked => return Err(RefreshError::ReauthenticationRequired),
        };
        match kimi::execute(request).await {
            kimi::Outcome::Complete(patch) => apply_if_current(storage, reserved, patch).await,
            kimi::Outcome::Throttled => {
                apply_if_current(
                    storage,
                    reserved,
                    Map::from_iter([(kimi::ATTEMPT.into(), Value::Null)]),
                )
                .await?;
                Err(RefreshError::ProviderUnavailable)
            }
            kimi::Outcome::Uncertain => Err(RefreshError::ReauthenticationRequired),
        }
    }

    async fn refresh_minimax_code(
        &self,
        storage: &Storage,
        account: Value,
        provider: Provider,
        region: minimax_code::Region,
    ) -> Result<Value, RefreshError> {
        let token =
            nonempty(&account["refreshToken"]).ok_or(RefreshError::ReauthenticationRequired)?;
        let marker = minimax_code::marker(token);
        if account[minimax_code::ATTEMPT] == marker {
            return Err(RefreshError::ReauthenticationRequired);
        }
        let request = minimax_code::prepare_refresh(storage, &account, region, token).await?;
        let reserved = match reserve_rotation(storage, account, provider, marker).await? {
            Reservation::Reserved(current) => current,
            Reservation::Changed(current) => return Ok(current),
            Reservation::Blocked => return Err(RefreshError::ReauthenticationRequired),
        };
        match minimax_code::refresh(request, &reserved).await {
            minimax_code::Rotation::Complete(patch) => {
                apply_if_current(storage, reserved, patch).await
            }
            minimax_code::Rotation::Throttled => {
                apply_if_current(
                    storage,
                    reserved,
                    Map::from_iter([(minimax_code::ATTEMPT.into(), Value::Null)]),
                )
                .await?;
                Err(RefreshError::ProviderUnavailable)
            }
            minimax_code::Rotation::Uncertain => Err(RefreshError::ReauthenticationRequired),
        }
    }

    async fn refresh_access(
        &self,
        provider: Provider,
        refresh: &str,
        route: Option<(&Storage, &Value, AccessEgress)>,
    ) -> Result<Map<String, Value>, RefreshError> {
        let config = &self.config;
        let routed;
        let binding = if let Some((storage, account, AccessEgress::Bound(relay))) = &route {
            let endpoint = match provider {
                Provider::GitHub => &config.github.token,
                Provider::IFlow => &config.iflow.token,
                _ => connection_test_endpoint(config, provider).ok_or(RefreshError::Unsupported)?,
            };
            let local = match provider {
                Provider::Claude => CLAUDE_TOKEN_FIXTURE,
                Provider::Codex => CODEX_TOKEN_FIXTURE,
                Provider::Gemini => GEMINI_TOKEN_FIXTURE,
                Provider::Antigravity => ANTIGRAVITY_TOKEN_FIXTURE,
                Provider::GitHub => GITHUB_TOKEN_FIXTURE,
                Provider::Cline | Provider::ClinePass => CLINE_FIXTURE,
                Provider::IFlow => config.iflow.token_url(),
                Provider::CodeBuddy(edition) => edition.refresh_fixture(),
                _ => return Err(RefreshError::Unsupported),
            };
            let direct = http_client(TIMEOUT)
                .build()
                .map_err(|_| RefreshError::ProviderUnavailable)?;
            let binding = match relay {
                Some(identity) => {
                    provider_http::bind_relay(
                        storage, account, endpoint, &direct, TIMEOUT, *identity,
                    )
                    .await
                }
                None => {
                    provider_http::bind(storage, account, endpoint, local, &direct, TIMEOUT, None)
                        .await
                }
            };
            Some(authenticated_binding(binding)?)
        } else {
            None
        };
        let (client, token_url) =
            if let Some((storage, account, AccessEgress::Vendor(protected))) = route {
                let url =
                    connection_test_endpoint(config, provider).ok_or(RefreshError::Unsupported)?;
                let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
                let delivery = Delivery::select(storage, &settings, account, url)
                    .await
                    .map_err(|_| RefreshError::Unsupported)?;
                if protected.is_some_and(|protected| protected != delivery.protected()) {
                    return Err(RefreshError::Unsupported);
                }
                let direct = http_client(TIMEOUT)
                    .build()
                    .map_err(|_| RefreshError::ProviderUnavailable)?;
                routed = delivery
                    .client(&direct, TIMEOUT)
                    .ok_or(RefreshError::ProviderUnavailable)?;
                let endpoint = if delivery.use_fixture() {
                    match provider {
                        Provider::Claude => CLAUDE_TOKEN_FIXTURE,
                        Provider::Codex => CODEX_TOKEN_FIXTURE,
                        Provider::Gemini => GEMINI_TOKEN_FIXTURE,
                        Provider::Antigravity => ANTIGRAVITY_TOKEN_FIXTURE,
                        Provider::Cline | Provider::ClinePass => CLINE_FIXTURE,
                        Provider::CodeBuddy(edition) => edition.refresh_fixture(),
                        Provider::Xai => "http://127.0.0.1:52989/xai-oauth/oauth2/token",
                        _ => return Err(RefreshError::Unsupported),
                    }
                } else {
                    url
                };
                (&routed, Some(endpoint))
            } else {
                (&self.client, None)
            };
        let post = |endpoint: &str| {
            binding.as_ref().map_or_else(
                || client.post(token_url.unwrap_or(endpoint)),
                |route| route.request(Method::POST),
            )
        };
        let request = match provider {
            Provider::Claude | Provider::Codex => {
                let (endpoint, client_id) = if provider == Provider::Claude {
                    (&config.claude.token, &config.claude.client)
                } else {
                    (&config.codex.token, &config.codex.client)
                };
                post(endpoint)
                    .header(header::ACCEPT, "application/json")
                    .json(&json!({"grant_type":"refresh_token","refresh_token":refresh,"client_id":client_id}))
            }
            Provider::Gemini | Provider::Antigravity => {
                let google = if provider == Provider::Gemini {
                    &config.google
                } else {
                    &config.antigravity
                };
                post(&google.token)
                    .header(header::ACCEPT, "application/json")
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh),
                        ("client_id", google.client.as_str()),
                        ("client_secret", google.secret.as_str()),
                    ])
            }
            Provider::GitHub => post(&config.github.token)
                .header(header::ACCEPT, "application/json")
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                    ("client_id", config.github.client.as_str()),
                ]),
            Provider::Cline | Provider::ClinePass => {
                let flow = if provider == Provider::Cline {
                    &config.cline
                } else {
                    &config.clinepass
                };
                let url = if let Some(endpoint) = token_url {
                    endpoint
                } else if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
                    && flow.refresh.starts_with("https://api.cline.bot/")
                {
                    CLINE_FIXTURE
                } else {
                    &flow.refresh
                };
                cline::refresh_request(post(url), refresh)
            }
            Provider::CodeBuddy(edition) => {
                if token_url.is_none() && binding.is_none() {
                    return Err(RefreshError::Unsupported);
                }
                edition
                    .refresh_request(post(edition.refresh_url()), refresh)
                    .ok_or(RefreshError::ReauthenticationRequired)?
            }
            Provider::IFlow => post(config.iflow.token_url())
                .header(header::ACCEPT, "application/json")
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                    ("client_id", config.iflow.client.as_str()),
                    ("client_secret", config.iflow.secret.as_str()),
                ])
                .basic_auth(&config.iflow.client, Some(&config.iflow.secret)),
            Provider::Xai => match token_url {
                Some(url) => config.xai.refresh_at(client, url, refresh),
                None => config.xai.refresh_request(&self.client, refresh).await,
            },
            Provider::Kiro
            | Provider::GrokCli
            | Provider::Nous
            | Provider::Kimi
            | Provider::Trae
            | Provider::MiniMaxCode
            | Provider::MiniMaxCodeGlobal => return Err(RefreshError::Unsupported),
        };
        let response = request
            .send()
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        if !response.status().is_success() {
            if matches!(provider, Provider::Codex | Provider::Xai)
                && let Ok(body) = response.text().await
                && match provider {
                    Provider::Codex => permanent_codex_error(&body),
                    Provider::Xai => {
                        body.contains("invalid_grant") || body.contains("invalid_request")
                    }
                    _ => false,
                }
            {
                return Err(RefreshError::ReauthenticationRequired);
            }
            return Err(RefreshError::ProviderUnavailable);
        }
        let data: Value = response
            .json()
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        let now = Utc::now();
        let (access, refresh, expires_in) =
            if matches!(provider, Provider::Cline | Provider::ClinePass) {
                let tokens = data
                    .get("data")
                    .filter(|value| !value.is_null())
                    .unwrap_or(&data);
                (
                    &tokens["accessToken"],
                    &tokens["refreshToken"],
                    cline_expiry(tokens, now),
                )
            } else if matches!(provider, Provider::CodeBuddy(_)) {
                if data["code"] != 0 {
                    return Err(RefreshError::ProviderUnavailable);
                }
                let tokens = &data["data"];
                (
                    &tokens["accessToken"],
                    &tokens["refreshToken"],
                    tokens["expiresIn"].clone(),
                )
            } else {
                (
                    &data["access_token"],
                    &data["refresh_token"],
                    data["expires_in"].clone(),
                )
            };
        let access = nonempty(access).ok_or(RefreshError::ProviderUnavailable)?;
        let mut patch = Map::from_iter([
            ("accessToken".into(), json!(access)),
            (
                "lastRefreshAt".into(),
                json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
            ),
        ]);
        if let Some(refresh) = nonempty(refresh) {
            patch.insert("refreshToken".into(), json!(refresh));
        }
        if matches!(provider, Provider::Codex | Provider::Xai)
            && let Some(id) = nonempty(&data["id_token"])
        {
            patch.insert("idToken".into(), json!(id));
        }
        if let Ok(expires) = expiry(now, Some(&expires_in))
            && !expires.is_null()
        {
            patch.insert("expiresAt".into(), expires);
            patch.insert("expiresIn".into(), expires_in);
        }
        Ok(patch)
    }

    async fn refresh_copilot(
        &self,
        github_access: &str,
        route: Option<(&Storage, &Value)>,
        relay: Option<[u8; 32]>,
    ) -> Result<Map<String, Value>, RefreshError> {
        let config = &self.config.github;
        let request = if let Some((storage, account)) = route {
            let direct = http_client(TIMEOUT)
                .build()
                .map_err(|_| RefreshError::ProviderUnavailable)?;
            let binding = if let Some(identity) = relay {
                provider_http::bind_relay(
                    storage,
                    account,
                    &config.copilot,
                    &direct,
                    TIMEOUT,
                    identity,
                )
                .await
            } else {
                provider_http::bind(
                    storage,
                    account,
                    &config.copilot,
                    COPILOT_TOKEN_FIXTURE,
                    &direct,
                    TIMEOUT,
                    None,
                )
                .await
            };
            authenticated_binding(binding)?.request(Method::GET)
        } else {
            self.client.get(&config.copilot)
        };
        let response = request
            .header(header::AUTHORIZATION, format!("token {github_access}"))
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, &config.refresh_user_agent)
            .header(
                "Editor-Version",
                format!("vscode/{}", config.vscode_version),
            )
            .header(
                "Editor-Plugin-Version",
                format!("copilot-chat/{}", config.chat_version),
            )
            .header("x-github-api-version", &config.refresh_api_version)
            .send()
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?
            .error_for_status()
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        let data: Value = response
            .json()
            .await
            .map_err(|_| RefreshError::ProviderUnavailable)?;
        let token = nonempty(&data["token"]).ok_or(RefreshError::ProviderUnavailable)?;
        let mut specifics = json!({"copilotToken": token});
        if data["expires_at"].as_i64().is_some_and(|epoch| epoch > 0) {
            specifics["copilotTokenExpiresAt"] = data["expires_at"].clone();
        }
        Ok(Map::from_iter([("providerSpecificData".into(), specifics)]))
    }
}

fn cline_expiry(tokens: &Value, now: DateTime<Utc>) -> Value {
    if let Some(expires) = tokens["expiresAt"].as_str().filter(|time| !time.is_empty()) {
        return DateTime::parse_from_rfc3339(expires).map_or(Value::Null, |expires| {
            json!(expires.signed_duration_since(now).num_seconds().max(1))
        });
    }
    ["expiresIn", "expires_in"]
        .into_iter()
        .map(|field| &tokens[field])
        .find(|value| connections::truthy(value))
        .map_or_else(|| json!(3600), Clone::clone)
}

async fn load(storage: &Storage, id: &str, provider: Provider) -> Result<Value, RefreshError> {
    let key = id.to_owned();
    let row = storage
        .run(false, move |db| connections::by_id(db, &key))
        .await
        .map_err(|_| RefreshError::Storage)?;
    row.filter(|account| available(account, provider))
        .ok_or(RefreshError::AccountUnavailable)
}

fn available(account: &Value, provider: Provider) -> bool {
    account["provider"] == provider.id()
        && account["authType"] == "oauth"
        && account["isActive"] == true
}

enum Reservation {
    Reserved(Value),
    Changed(Value),
    Blocked,
}

fn attempt_field(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Kiro => Some(kiro::ATTEMPT),
        Provider::GrokCli => Some(grok::ATTEMPT),
        Provider::Nous => Some(nous::ATTEMPT),
        Provider::Kimi => Some(kimi::ATTEMPT),
        Provider::MiniMaxCode | Provider::MiniMaxCodeGlobal => Some(minimax_code::ATTEMPT),
        _ => None,
    }
}

async fn reserve_rotation(
    storage: &Storage,
    observed: Value,
    provider: Provider,
    marker: String,
) -> Result<Reservation, RefreshError> {
    let attempt = attempt_field(provider).ok_or(RefreshError::Unsupported)?;
    let result = storage
        .run(true, move |database| {
            let tx = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(id) = observed["id"].as_str() else {
                return Ok(None);
            };
            let Some(mut current) =
                connections::by_id(&tx, id)?.filter(|account| available(account, provider))
            else {
                return Ok(None);
            };
            if !same_access_credentials(&observed, &current) {
                return Ok(Some(Reservation::Changed(current)));
            }
            if current[attempt] == marker {
                return Ok(Some(Reservation::Blocked));
            }
            current[attempt] = json!(marker);
            connections::write(&tx, &current)?;
            tx.commit()?;
            Ok(Some(Reservation::Reserved(current)))
        })
        .await
        .map_err(|_| RefreshError::Storage)?;
    result.ok_or(RefreshError::AccountUnavailable)
}

async fn apply_if_current(
    storage: &Storage,
    comparison: Value,
    mut patch: Map<String, Value>,
) -> Result<Value, RefreshError> {
    let stored = storage
        .run(true, move |database| {
            let tx = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(id) = comparison["id"].as_str() else {
                return Ok(None);
            };
            let Some(current) = connections::by_id(&tx, id)? else {
                return Ok(None);
            };
            let Some(provider) = nonempty(&comparison["provider"]).and_then(Provider::from_id)
            else {
                return Ok(None);
            };
            if !available(&current, provider) {
                return Ok(None);
            }
            if !same_access_credentials(&comparison, &current)
                || attempt_field(provider).is_some_and(|field| {
                    patch.contains_key(field) && current[field] != comparison[field]
                })
                || (patch.contains_key("providerSpecificData")
                    && !same_copilot_credentials(&comparison, &current))
            {
                return Ok(Some(current));
            }
            if let Some(metadata) = patch.get_mut("providerSpecificData") {
                let mut merged = connections::object(current["providerSpecificData"].clone());
                merged.extend(connections::object(std::mem::take(metadata)));
                *metadata = Value::Object(merged);
            }
            let refreshed = connections::patch(current, patch, &connections::now());
            connections::write(&tx, &refreshed)?;
            tx.commit()?;
            Ok(Some(refreshed))
        })
        .await
        .map_err(|_| RefreshError::Storage)?;
    stored.ok_or(RefreshError::AccountUnavailable)
}

fn fingerprint(account: &Value) -> [u8; 32] {
    let fields = CREDENTIAL_FIELDS
        .iter()
        .map(|name| &account[*name])
        .chain(
            COPILOT_FIELDS
                .iter()
                .chain(std::iter::once(&"lastRefreshAt"))
                .map(|name| &account["providerSpecificData"][*name]),
        )
        .chain(
            context_fields(account)
                .iter()
                .map(|name| &account["providerSpecificData"][*name]),
        )
        .chain((account["provider"] == "nous").then_some(&account["apiKey"]))
        .chain(
            kiro_legacy_fields(account)
                .iter()
                .map(|name| &account[*name]),
        );
    let mut hash = Sha256::new();
    for field in fields {
        let value = field.to_string();
        hash.update(value.len().to_le_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

fn same_credentials(first: &Value, second: &Value) -> bool {
    same_access_credentials(first, second) && same_copilot_credentials(first, second)
}

fn same_access_credentials(first: &Value, second: &Value) -> bool {
    if first["provider"] == "nous" && first["apiKey"] != second["apiKey"] {
        return false;
    }
    CREDENTIAL_FIELDS
        .iter()
        .all(|field| first[*field] == second[*field])
        && first["providerSpecificData"]["lastRefreshAt"]
            == second["providerSpecificData"]["lastRefreshAt"]
        && context_fields(first).iter().all(|field| {
            first["providerSpecificData"][*field] == second["providerSpecificData"][*field]
        })
        && kiro_legacy_fields(first)
            .iter()
            .all(|name| first[*name] == second[*name])
}

fn kiro_legacy_fields(account: &Value) -> &'static [&'static str] {
    if account["provider"] == "kiro" {
        kiro::LEGACY_FIELDS
    } else {
        &[]
    }
}

fn context_fields(account: &Value) -> &'static [&'static str] {
    match account["provider"].as_str() {
        Some("kiro") => kiro::CONTEXT,
        Some("grok-cli") => grok::CONTEXT,
        Some("nous") => nous::CONTEXT,
        Some("kimi") => kimi::CONTEXT,
        Some("minimax-code" | "minimax-code-global") => minimax_code::CONTEXT,
        _ => &[],
    }
}

fn same_copilot_credentials(first: &Value, second: &Value) -> bool {
    COPILOT_FIELDS.iter().all(|field| {
        first["providerSpecificData"][*field] == second["providerSpecificData"][*field]
    })
}

fn should_refresh(provider: Provider, account: &Value, now_ms: i64) -> bool {
    let missing_access = match provider {
        Provider::Codex | Provider::Gemini => nonempty(&account["accessToken"]).is_none(),
        Provider::GitHub => crate::github_executor::token(account).is_none(),
        _ => false,
    };
    if missing_access && nonempty(&account["refreshToken"]).is_some() {
        return true;
    }
    let lead = refresh_lead(provider);
    let expiry = time_ms(or_nonnull(
        &account["expiresAt"],
        &account["tokenExpiresAt"],
    ));
    if expiry.is_some_and(|expiry| expiry.saturating_sub(now_ms) < lead) {
        return true;
    }
    if provider != Provider::Codex || nonempty(&account["refreshToken"]).is_none() {
        return false;
    }
    let last = time_ms(or_nonnull(
        &account["lastRefreshAt"],
        or_nonnull(
            &account["lastRefresh"],
            &account["providerSpecificData"]["lastRefreshAt"],
        ),
    ));
    last.is_none_or(|last| now_ms.saturating_sub(last) >= CODEX_MAX_AGE_MS)
}

fn refresh_lead(provider: Provider) -> i64 {
    match provider {
        Provider::Claude => CLAUDE_LEAD_MS,
        Provider::Codex => CODEX_LEAD_MS,
        Provider::MiniMaxCode | Provider::MiniMaxCodeGlobal => MINIMAX_CODE_LEAD_MS,
        Provider::IFlow => 86_400_000,
        _ => DEFAULT_LEAD_MS,
    }
}

fn copilot_due(account: &Value, now_ms: i64) -> bool {
    let details = &account["providerSpecificData"];
    nonempty(&details["copilotToken"]).is_none()
        || time_ms(&details["copilotTokenExpiresAt"])
            .is_none_or(|expiry| expiry.saturating_sub(now_ms) < DEFAULT_LEAD_MS)
}

fn or_nonnull<'a>(value: &'a Value, fallback: &'a Value) -> &'a Value {
    if value.is_null() { fallback } else { value }
}

pub(super) fn time_ms(value: &Value) -> Option<i64> {
    if let Some(epoch) = value.as_i64() {
        return Some(if epoch < 1_000_000_000_000 {
            epoch.saturating_mul(1000)
        } else {
            epoch
        });
    }
    if let Some(epoch) = value.as_f64() {
        let millis = if epoch < 1_000_000_000_000.0 {
            epoch * 1000.0
        } else {
            epoch
        };
        return (millis >= i64::MIN as f64 && millis < i64::MAX as f64).then_some(millis as i64);
    }
    if let Some(raw) = value.as_str() {
        return DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|time| time.timestamp_millis());
    }
    None
}

fn permanent_codex_error(body: &str) -> bool {
    let json: Value = serde_json::from_str(body).unwrap_or_default();
    let marker = |text: &str| {
        let text = text.to_ascii_lowercase();
        [
            "refresh_token_expired",
            "refresh_token_reused",
            "refresh_token_invalidated",
            "invalid_grant",
        ]
        .iter()
        .any(|marker| text.contains(marker))
    };
    [
        json["error"]["code"].as_str(),
        json["error"].as_str(),
        json["error_code"].as_str(),
        json["error_description"]
            .as_str()
            .or_else(|| json["message"].as_str())
            .or(Some(body)),
    ]
    .into_iter()
    .flatten()
    .any(marker)
}
