mod process {
    include!(env!("HEADROOM_MACOS_MODULE"));

    pub(super) fn fixture(args: &[String]) -> Value {
        let launcher = Launcher {
            command: PathBuf::from(&args[0]),
            uid: unsafe { libc::geteuid() },
            startup: Duration::from_millis(400),
        };
        let root = Path::new(&args[2]);
        let answer = match args[1].as_str() {
            "status" => {
                let pid = launcher
                    .agent(root, false)
                    .ok()
                    .flatten()
                    .and_then(|agent| agent.owned_job().ok().flatten())
                    .and_then(|(_, job)| job.pid);
                return json!({"managedPid":pid});
            }
            "stop" => launcher.stop(root),
            "start" | "restart" => launcher.start(
                root,
                (args[3] != "-").then(|| PathBuf::from(&args[3])),
                18787,
                true,
                false,
                args[1] == "restart",
            ),
            action => panic!("unexpected action {action}"),
        };
        match answer {
            Ok(value) => value,
            Err(error) => json!({"status":error.status.as_u16(),"code":error.code,"error":error.message}),
        }
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let _native = (process::start, process::stop, process::managed_pid);
    println!("{}", process::fixture(&args));
}
