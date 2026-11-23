#[cfg(target_os = "linux")]
mod benchmark;
#[cfg(target_os = "linux")]
mod fixture;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    match benchmark::run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("The 9Router gateway benchmark requires Linux.");
    std::process::ExitCode::FAILURE
}
