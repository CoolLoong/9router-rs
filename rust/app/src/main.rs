mod account_quota;
mod anthropic_request;
mod anthropic_tokens;
mod anthropic_upstream;
mod antigravity_quota_cache;
mod antigravity_transport;
mod azure_transport;
mod background_tokens;
mod bedrock_transport;
mod capacity_adapter;
mod catalog_guard;
mod chat;
mod chat_accounts;
mod chat_bypass;
mod chat_client;
mod chat_format;
mod chat_input;
mod chat_output;
mod chat_prefetch;
mod chat_protocol_input;
mod chat_usage;
mod chatgpt_web;
mod chatgpt_web_delta;
mod claude_passthrough;
mod claude_settings;
mod claude_sse;
mod claude_thinking;
mod claude_transport;
mod cli;
mod cli_connect;
#[cfg(target_os = "linux")]
mod cli_lifecycle;
#[cfg(target_os = "macos")]
mod cli_macos_launch;
mod cli_tools;
#[cfg(target_os = "linux")]
mod cli_tray;
#[cfg(target_os = "macos")]
mod cli_tray_macos;
#[cfg(target_os = "windows")]
mod cli_tray_windows;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod cli_tui;
mod cli_video;
mod cline_transport;
mod cloudflare_tunnel;
mod codebuddy_device;
mod codex_executor;
mod codex_import;
mod codex_settings;
mod combo_chat;
mod combos;
mod commandcode_identity;
mod commandcode_transport;
mod console_logs;
mod copilot_models;
mod cursor_chat;
mod cursor_identity;
mod cursor_models;
mod dashboard_session;
mod dashboard_ui;
mod database_admin;
mod deepseek_web;
mod devin_acp;
mod droid_settings;
mod embeddings;
mod fusion;
mod gemini_cli_transport;
mod gemini_live_stt;
#[path = "units/gemini_model_list.rs"]
mod gemini_model_list;
mod gemini_public;
mod gemini_request;
mod gemini_schema;
mod gemini_signatures;
mod gemini_tts;
mod gemini_upstream;
mod gemini_web;
mod github_claude;
mod github_executor;
mod github_responses;
mod gitlab_transport;
mod glm_device;
mod google_code_assist;
mod grok_cli_body;
mod grok_cli_chat;
mod grok_cli_http;
mod grok_cli_models;
mod grok_web;
mod grok_web_media;
mod headroom;
mod headroom_admin;
mod iflow_transport;
mod image_generation;
mod image_local;
mod key_identity;
mod key_policy;
mod keys;
mod kimchi_chat;
mod kimchi_models;
mod kimchi_oauth;
mod kimi_device;
mod kiro_auth;
mod kiro_chat;
mod kiro_device;
mod kiro_models;
mod local_device_tts;
mod mcp_bridge;
mod mimo_account;
mod mimo_web_login;
mod mitm_antigravity;
mod mitm_listener;
mod model_availability;
mod model_catalog_sync;
mod model_info;
mod model_kv;
mod model_resolution;
mod muse_device;
mod muse_web;
#[cfg(all(
    any(target_os = "linux", target_os = "macos", windows),
    feature = "native-package"
))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod native_update_source;
#[cfg(all(target_os = "linux", feature = "native-package"))]
mod native_updater;
#[cfg(all(windows, feature = "native-package"))]
mod native_updater_windows;
mod nous_device;
mod nous_management;
mod observability;
mod observed_response;
mod oidc;
mod ollama_local;
mod ollama_public;
mod openai_models;
mod openai_thinking;
mod opencode_chat;
mod perplexity_web_chat;
mod pricing_api;
mod provider_connections;
mod provider_http;
mod provider_imports;
mod provider_index;
mod provider_nodes;
mod provider_oauth;
mod provider_overrides;
mod provider_tools;
mod providers;
mod proxy_pools;
mod proxy_transport;
mod pxpipe_admin;
mod pxpipe_factsheet;
mod pxpipe_render;
mod pxpipe_runtime;
mod pxpipe_static;
mod pxpipe_transform;
mod qoder_chat;
mod qoder_device;
mod qoder_models;
mod quota_auto_ping;
mod quota_ping_schedule;
mod responses_request;
mod responses_sse;
mod responses_to_chat;
mod responses_upstream;
#[path = "units/router_health.rs"]
mod router_health;
mod rtk_compression;
mod rtk_filters;
mod saml;
mod settings;
mod shutdown_api;
mod smart_router;
mod speech;
mod speech_providers;
mod speech_to_text;
mod sse_reader;
mod static_chat;
mod storage;
mod storage_migration;
mod support_api;
mod system_prompts;
mod systemone;
mod tailscale_api;
mod thinking_catalog;
mod thinking_intent;
mod trae_chat;
mod uc_media;
mod upstream;
mod usage_activity;
mod usage_dashboard;
mod veoaifree_media;
mod vertex_transport;
mod vertex_video;
mod videos;
mod voices;
mod web_gateway;
#[cfg(all(windows, feature = "native-package"))]
#[path = "../../native-package/windows-update-acl.rs"]
mod windows_acl;
#[cfg(all(windows, feature = "native-package"))]
#[path = "../../native-package/windows-update-handoff.rs"]
mod windows_handoff;
#[cfg(all(windows, feature = "native-package"))]
#[path = "../../native-package/windows-update-process.rs"]
mod windows_process;
mod windsurf_chat;
mod xiaomi_mimo_oauth;
mod zed_transport;

use std::net::SocketAddr;
use std::path::PathBuf;

fn main() {
    #[cfg(target_os = "linux")]
    if let Some(code) = tailscale_api::daemon_entry() {
        std::process::exit(code);
    }
    run();
}

fn core_routes() -> axum::Router {
    use axum::routing::get;

    axum::Router::new()
        .route(
            "/api/health",
            get(router_health::handle).options(router_health::handle),
        )
        .route(
            "/api/v1beta/models",
            get(gemini_model_list::handle).options(gemini_model_list::handle),
        )
        .route(
            "/v1beta/models",
            get(gemini_model_list::handle).options(gemini_model_list::handle),
        )
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn run() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(code) = devin_acp::client_tools_stdio(&args).await {
        std::process::exit(code);
    }
    #[cfg(all(windows, feature = "native-package"))]
    let (args, windows_child) = {
        let mut args = args;
        let child = native_updater_windows::parse_entry(&mut args).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(1);
        });
        (args, child)
    };
    let options = match cli::dispatch(&args) {
        Ok(cli::Invocation::Server(options)) => {
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            if options.tray {
                eprintln!("System tray mode is unavailable on this platform");
                std::process::exit(1);
            }
            options
        }
        Ok(cli::Invocation::Connect(args)) => {
            let status = cli_connect::run(&args).await.unwrap_or_else(|| {
                eprintln!(
                    "Could not resolve the local configuration directory or connect arguments"
                );
                1
            });
            std::process::exit(status)
        }
        Ok(cli::Invocation::XaiVideo(args)) => std::process::exit(cli_video::run(&args).await),
        Err(code) => std::process::exit(code),
    };
    if let Some(path) = std::env::var_os("RUST_ANYTHING_FALLBACK_LOG") {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
    }
    cli_tools::mitm::register_listener(mitm_listener::configure);
    if options.show_log {
        console_logs::enable_terminal();
    }
    let port = options
        .port
        .or_else(|| {
            std::env::var("PORT")
                .ok()
                .and_then(|port| port.parse().ok())
        })
        .unwrap_or(20128);
    let host = options.host.as_deref().unwrap_or("127.0.0.1");
    let root = runtime_data_dir();
    #[cfg(all(windows, feature = "native-package"))]
    let root = windows_child
        .as_ref()
        .map(|child| child.root.clone())
        .or(root);
    #[cfg(all(windows, feature = "native-package"))]
    let windows_runtime = native_updater_windows::Runtime::early(root.as_deref(), windows_child)
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(1);
        });
    #[cfg(target_os = "linux")]
    let (listener, mut lease) = if let Some(path) = &root {
        #[cfg(all(target_os = "linux", feature = "native-package"))]
        let reserve = if std::env::var(native_updater::CHILD).as_deref() == Ok("1") {
            cli_lifecycle::reserve_update_child(path, host, port).await
        } else {
            cli_lifecycle::reserve(path, host, port).await
        };
        #[cfg(not(all(target_os = "linux", feature = "native-package")))]
        let reserve = cli_lifecycle::reserve(path, host, port).await;
        match reserve {
            Ok((listener, lease)) => (listener, Some(lease)),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    } else {
        (
            tokio::net::TcpListener::bind((host, port))
                .await
                .expect("binding the port"),
            None,
        )
    };
    #[cfg(not(target_os = "linux"))]
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .expect("binding the port");
    let storage = match root {
        Some(path) => storage::Storage::open(path),
        None => storage::Storage::temporary(),
    }
    .expect("initializing SQLite storage");
    background_tokens::start(storage.clone());
    let recorder = observability::Recorder::new(storage.clone());
    let pxpipe = pxpipe_runtime::Runtime::new(storage.root());
    if let Ok(address) = listener.local_addr() {
        console_logs::record("INFO", &format!("9Router listening on {address}"));
    }
    let requested_shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let mcp = mcp_bridge::Manager::default();
    let bound = listener.local_addr().expect("listener address");
    #[cfg(all(target_os = "linux", feature = "native-package"))]
    let native_updates =
        native_updater::Updater::new(storage.clone(), requested_shutdown.clone(), lease.is_some());
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", windows),
        feature = "native-package"
    ))]
    native_update_source::discover_at_startup(
        options.skip_update || std::env::var(native_update_source::CHILD).as_deref() == Ok("1"),
    );
    #[cfg(all(windows, feature = "native-package"))]
    let windows_update = windows_runtime.activate(
        storage.clone(),
        requested_shutdown.clone(),
        bound,
        args.clone(),
    );
    let tailscale = tailscale_api::Daemon::default();
    let cloudflare = cloudflare_tunnel::Manager::new(storage.clone(), bound.port());
    cloudflare.watch();
    let catalog_sync = model_catalog_sync::CatalogSync::new(storage.clone());
    catalog_sync.start_scheduler();
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let terminal = (!options.tray && cli_tui::available()).then(|| storage.root().to_owned());
    #[cfg(target_os = "linux")]
    let tray_host = host.to_owned();
    let (cli_tools_router, mitm_shutdown) = cli_tools::router(storage.clone(), bound);
    let app = core_routes()
        .merge(settings::router(storage.clone()))
        .merge(dashboard_session::router(storage.clone()))
        .merge(oidc::router(storage.clone()))
        .merge(dashboard_ui::router(storage.clone()))
        .merge(database_admin::router(storage.clone()))
        .merge(keys::router(storage.clone()))
        .merge(support_api::router(storage.clone()))
        .merge(console_logs::router(storage.clone()))
        .merge(shutdown_api::router(
            storage.clone(),
            requested_shutdown.clone(),
        ))
        .merge(cli_tools_router)
        .merge(droid_settings::router(storage.clone()))
        .merge(codex_settings::router(storage.clone()))
        .merge(claude_settings::router(storage.clone()))
        .merge(combos::router(storage.clone()))
        .merge(model_kv::router(storage.clone()))
        .merge(model_availability::router(storage.clone()))
        .merge(catalog_sync.router())
        .merge(providers::router(storage.clone()))
        .merge(provider_nodes::router(storage.clone()))
        .merge(mcp_bridge::router(storage.clone(), mcp.clone()))
        .merge(cloudflare_tunnel::router(
            cloudflare.clone(),
            tailscale.clone(),
        ))
        .merge(tailscale_api::router(
            storage.clone(),
            bound,
            tailscale.clone(),
        ))
        .merge(provider_oauth::router(storage.clone()))
        .merge(provider_imports::router(storage.clone()))
        .merge(xiaomi_mimo_oauth::router(storage.clone()))
        .merge(mimo_web_login::router(storage.clone()))
        .merge(kiro_auth::router(storage.clone()))
        .merge(kiro_device::router(storage.clone()))
        .merge(kimi_device::router(storage.clone()))
        .merge(nous_device::router(storage.clone()))
        .merge(qoder_device::router(storage.clone()))
        .merge(glm_device::router(storage.clone()))
        .merge(kimchi_oauth::router(storage.clone()))
        .merge(muse_device::router(storage.clone()))
        .merge(codebuddy_device::router(storage.clone()))
        .merge(codex_import::router(storage.clone()))
        .merge(provider_tools::router(
            storage.clone(),
            listener.local_addr().expect("listener address"),
        ))
        .merge(proxy_pools::router(storage.clone()))
        .merge(pxpipe_admin::router(storage.clone(), pxpipe.clone()))
        .merge(headroom_admin::router(storage.clone()))
        .merge(openai_models::router(storage.clone()))
        .merge(embeddings::router(storage.clone()))
        .merge(speech::router(storage.clone()))
        .merge(speech_to_text::router(storage.clone()))
        .merge(image_generation::router(storage.clone()))
        .merge(videos::router(storage.clone()))
        .merge(voices::router(storage.clone()))
        .merge(web_gateway::router(storage.clone()))
        .merge(systemone::router(storage.clone()))
        .merge(chat::router(storage.clone(), recorder.clone(), pxpipe))
        .merge(anthropic_tokens::router())
        .merge(usage_dashboard::router(storage.clone()))
        .merge(account_quota::router(
            storage.clone(),
            quota_auto_ping::start,
        ))
        .merge(usage_activity::router(storage.clone()));
    #[cfg(all(target_os = "linux", feature = "native-package"))]
    let app = app.merge(native_updates.router());
    #[cfg(all(windows, feature = "native-package"))]
    let app = app
        .merge(windows_update.router())
        .layer(axum::middleware::from_fn_with_state(
            windows_update,
            native_updater_windows::Updater::gate,
        ));
    let app = app.layer(axum::middleware::from_fn_with_state(
        storage,
        catalog_guard::enforce_gateway_access,
    ));
    let app = if options.show_log {
        app.layer(axum::middleware::from_fn(console_logs::request))
    } else {
        app
    };
    #[cfg(target_os = "linux")]
    let _hangup = options
        .tray
        .then(|| tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()))
        .transpose()
        .expect("registering tray signal handler");
    #[cfg(target_os = "linux")]
    let tray = if options.tray {
        Some(cli_tray::start(bound.port(), &tray_host).await)
    } else {
        None
    };
    #[cfg(target_os = "windows")]
    let tray = if options.tray {
        Some(cli_tray_windows::start(bound).await)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let macos_shutdown = requested_shutdown.clone();
    let shutdown = async move {
        tokio::select! {
            _ = shutdown_signal() => {}
            _ = requested_shutdown.notified() => {}
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(lease) = lease.as_mut()
        && let Err(error) = lease.publish()
    {
        eprintln!("{error}");
        return;
    }
    mitm_shutdown.watch_recovery();
    let stop = async move {
        async move {
            tokio::pin!(shutdown);
            #[cfg(any(target_os = "linux", windows))]
            let tray = if let Some(directory) = terminal {
                let exit = tokio::select! {
                    _ = &mut shutdown => return,
                    exit = cli_tui::run(bound, &directory) => exit,
                };
                match exit {
                    Ok(cli_tui::Exit::Quit) => return,
                    Ok(cli_tui::Exit::Hide) => {
                        #[cfg(target_os = "linux")]
                        {
                            let _ = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::hangup(),
                            );
                            let tray = cli_tray::start(bound.port(), &tray_host).await;
                            if tray.is_registered()
                                && let Err(error) = cli_tray::enable_autostart()
                            {
                                eprintln!("{error}");
                            }
                            Some(tray)
                        }
                        #[cfg(windows)]
                        {
                            let tray = cli_tray_windows::start(bound).await;
                            if tray.is_registered() {
                                if let Err(error) = cli_tray_windows::enable_autostart() {
                                    eprintln!("{error}");
                                }
                                if let Err(error) = cli_tray_windows::detach_console() {
                                    eprintln!("Could not detach from the Windows console: {error}");
                                }
                            }
                            Some(tray)
                        }
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        return;
                    }
                }
            } else {
                tray
            };
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if let Some(mut tray) = tray {
                tokio::select! {
                    _ = tray.wait() => {}
                    _ = &mut shutdown => {}
                }
                tray.stop().await;
                return;
            }
            shutdown.await;
        }
        .await;
        mitm_shutdown.shutdown().await;
        console_logs::shutdown();
        usage_activity::shutdown();
        mcp.shutdown().await;
        cloudflare.shutdown().await;
    };
    let serve = async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop)
        .await;
        #[cfg(target_os = "linux")]
        tailscale.shutdown().await;
        recorder.flush().await;
        served
    };
    #[cfg(target_os = "macos")]
    if options.tray || terminal.is_some() {
        cli_macos_launch::run(serve, bound, terminal.as_deref(), macos_shutdown)
            .await
            .expect("serving");
        return;
    }
    serve.await.expect("serving");
    #[cfg(all(target_os = "linux", feature = "native-package"))]
    {
        drop(lease);
        if let Err(error) = native_updates.apply(bound, &args).await {
            eprintln!("{error}");
        }
    }
}

fn runtime_data_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DATA_DIR").filter(|path| !path.is_empty()) {
        #[cfg(windows)]
        if path.to_string_lossy().starts_with('/') {
            return windows_data_dir();
        }
        return Some(path.into());
    }
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        || std::env::var_os("RUST_ANYTHING_FALLBACK_LOG").is_some()
    {
        return None;
    }
    #[cfg(windows)]
    return windows_data_dir();
    #[cfg(not(windows))]
    std::env::home_dir().map(|home| home.join(".9router"))
}

#[cfg(windows)]
fn windows_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join("AppData/Roaming")))
        .map(|appdata| appdata.join("9router"))
}
