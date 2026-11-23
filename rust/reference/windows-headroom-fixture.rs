use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, ExitCode};

fn main() -> io::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("proxy") => {
            assert_eq!(args[1], "--port");
            let executable = std::env::current_exe()?;
            if executable.with_file_name("exit-early").exists() {
                return Ok(ExitCode::from(42));
            }
            println!("{}", args.join("|"));
            let status = Command::new(executable).args(["serve", &args[2]]).status()?;
            Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
        }
        Some("serve") => {
            let port = args[1].parse::<u16>().expect("fixture port");
            let stream = TcpListener::bind(("127.0.0.1", port))?;
            for client in stream.incoming() {
                let mut client = client?;
                let mut input = BufReader::new(&client);
                let mut line = String::new();
                input.read_line(&mut line)?;
                let stop = line.starts_with("POST /quit ");
                loop {
                    let mut header = String::new();
                    if input.read_line(&mut header)? == 0 || header == "\r\n" {
                        break;
                    }
                }
                let body = format!("{{\"fixturePid\":{}}}", std::process::id());
                write!(
                    client,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
                if stop {
                    break;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some("write-pid") => {
            std::fs::write(&args[1], &args[2])?;
            Ok(ExitCode::SUCCESS)
        }
        _ => Ok(ExitCode::from(2)),
    }
}
