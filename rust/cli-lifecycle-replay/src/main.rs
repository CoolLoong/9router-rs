#[path = "../../app/src/cli_lifecycle.rs"]
mod cli_lifecycle;

use tokio::signal::unix::{SignalKind, signal};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let root = std::path::PathBuf::from(args.next().unwrap());
    let port: u16 = args.next().unwrap().into_string().unwrap().parse().unwrap();
    let mode = args.next().unwrap().into_string().unwrap();
    let host = args.next().unwrap().into_string().unwrap();
    let mut term = signal(SignalKind::terminate())?;
    let (listener, mut lease) = cli_lifecycle::reserve(&root, &host, port).await?;
    if mode != "unpublished" {
        lease.publish()?;
    }
    println!("ready {}", listener.local_addr()?.port());
    term.recv().await;
    if mode == "stubborn" {
        std::future::pending::<()>().await;
    }
    Ok(())
}
