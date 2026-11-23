#[allow(dead_code)]
#[path = "../../app/src/cli.rs"]
mod cli;
#[allow(dead_code)]
#[path = "../../app/src/cli_connect.rs"]
mod cli_connect;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::io::IsTerminal;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().expect("fixture cwd");
    let data = cwd.join("fixture-data");
    let home = cwd.join("fixture-home");
    let password = std::env::var("NINE_ROUTER_PASSWORD")
        .ok()
        .filter(|password| !password.is_empty());
    let tty = std::io::stdin().is_terminal();
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    let result = cli_connect::run_in(
        &args,
        &home,
        &data,
        password,
        "fixture-host",
        tty,
        config_home.as_deref(),
    )
    .await;
    std::process::ExitCode::from(result.unwrap_or(125) as u8)
}
