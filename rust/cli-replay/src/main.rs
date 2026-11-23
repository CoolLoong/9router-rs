#[allow(dead_code)]
#[path = "../../app/src/cli.rs"]
mod cli;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().expect("fixture cwd");
    let home = cwd.join("fixture-home");
    let xdg = std::env::var_os("ROUTER_CLI_XDG_FIXTURE").map(|_| cwd.join("fixture-xdg"));
    let launch = std::env::var("ROUTER_CLI_STARTUP_FIXTURE").ok();
    match cli::dispatch_in(
        &args,
        Some(&home),
        xdg.as_deref(),
        launch.as_deref() == Some("tty"),
    ) {
        Err(status) => std::process::ExitCode::from(status as u8),
        Ok(cli::Invocation::Server(options)) if launch.is_some() => {
            println!(
                "{}",
                serde_json::json!({
                    "port":options.port.unwrap_or(20128),
                    "host":options.host.unwrap_or_else(|| "0.0.0.0".into()),
                    "noBrowser":options.no_browser,
                    "skipUpdate":options.skip_update,
                    "showLog":options.show_log,
                    "trayMode":options.tray,
                })
            );
            std::process::ExitCode::SUCCESS
        }
        Ok(_) => std::process::ExitCode::from(2),
    }
}
