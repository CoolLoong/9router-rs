#[allow(dead_code)]
#[path = "../../app/src/cli.rs"]
mod cli;
#[path = "../../app/src/cli_tui.rs"]
mod cli_tui;
#[allow(dead_code)]
#[path = "../../app/src/key_identity.rs"]
mod key_identity;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let address = std::env::args().nth(1).unwrap().parse().unwrap();
    let state = std::env::var_os("DATA_DIR").unwrap();
    match cli_tui::run(address, std::path::Path::new(&state)).await? {
        cli_tui::Exit::Quit => Ok(()),
        cli_tui::Exit::Hide => Err(std::io::Error::other("fixture selected Hide")),
    }
}
