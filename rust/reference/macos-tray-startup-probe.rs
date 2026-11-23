#[path = "../app/src/cli_tray_mac_startup.rs"]
mod startup;

use std::path::Path;

fn main() -> std::io::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let [action, home, launchctl, exe] = arguments.as_slice() else {
        return Err(std::io::Error::other("usage: probe status|toggle|enable home launchctl executable"));
    };
    let startup = startup::Startup::new(Path::new(home), 501, Path::new(launchctl));
    if action == "toggle" {
        startup.toggle(Path::new(exe))?;
    } else if action == "enable" {
        startup.enable(Path::new(exe))?;
    } else if action != "status" {
        return Err(std::io::Error::other("unexpected action"));
    }
    println!("{}", startup.enabled()?);
    Ok(())
}
