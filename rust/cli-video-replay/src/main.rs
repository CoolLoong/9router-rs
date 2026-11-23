#[path = "../../app/src/cli_video.rs"]
mod cli_video;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(cli_video::run(&args).await as u8)
}
