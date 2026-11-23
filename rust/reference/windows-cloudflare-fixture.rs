use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = if args.first().is_some_and(|arg| arg == "tunnel") {
        assert_eq!(args.len(), 8);
        assert_eq!(args[1], "--url");
        assert!(args[2].starts_with("http://127.0.0.1:"));
        assert_eq!(args[3], "--config");
        assert_eq!(std::fs::read_to_string(&args[4])?, "{}\n");
        assert_eq!(args[5..], ["--no-autoupdate", "--retries", "99"]);
        let data = PathBuf::from(std::env::var_os("DATA_DIR").expect("fixture data"));
        std::fs::write(data.join("cloudflared-arguments.txt"), args.join("\n"))?;
        std::env::var("FIXTURE_CLOUDFLARED_PORT").expect("fixture port")
    } else if let [command, port] = args.as_slice()
        && command == "serve"
    {
        port.clone()
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unexpected fixture arguments"));
    };
    let listener = TcpListener::bind(("127.0.0.1", port.parse::<u16>().expect("local fixture port")))?;
    if args[0] == "tunnel" {
        println!("https://fixture-cutover.trycloudflare.com");
        io::stdout().flush()?;
    }
    for connection in listener.incoming() {
        let mut connection = connection?;
        let mut input = BufReader::new(&connection);
        let mut line = String::new();
        input.read_line(&mut line)?;
        let quit = line.starts_with("POST /quit ");
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
        }
        let body = format!("{{\"pid\":{}}}", std::process::id());
        write!(
            connection,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        if quit {
            break;
        }
    }
    Ok(())
}
