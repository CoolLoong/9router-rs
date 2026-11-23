use crate::fixture::{self, MockCalls};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{self, MissedTickBehavior};

const BIND: &str = "127.0.0.1:0";
const READY_POLL: Duration = Duration::from_millis(10);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const RSS_INTERVAL: Duration = Duration::from_millis(20);
const USAGE: &str =
    "Compare original 9Router and 9router-rs with a local chat-completions workload.

Usage: nine-router-benchmark --node-server /path/to/app/server.js --rust-bin /path/to/9router
       [--rounds 5] [--requests 1000] [--concurrency 8]
";

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Serialize)]
struct Options {
    node_server: PathBuf,
    rust_bin: PathBuf,
    rounds: usize,
    requests: usize,
    concurrency: usize,
}

impl Options {
    fn parse() -> Result<Option<Self>> {
        let mut args = std::env::args_os().skip(1);
        let mut node_server = None;
        let mut rust_bin = None;
        let mut rounds = 5;
        let mut requests = 1000;
        let mut concurrency = 8;
        while let Some(arg) = args.next() {
            let flag = arg.to_string_lossy();
            if matches!(flag.as_ref(), "-h" | "--help") {
                print!("{USAGE}");
                return Ok(None);
            }
            let value = || format!("{flag} requires a value");
            match flag.as_ref() {
                "--node-server" => node_server = args.next().map(PathBuf::from),
                "--rust-bin" => rust_bin = args.next().map(PathBuf::from),
                "--rounds" => rounds = positive(args.next().ok_or_else(value)?, &flag)?,
                "--requests" => requests = positive(args.next().ok_or_else(value)?, &flag)?,
                "--concurrency" => concurrency = positive(args.next().ok_or_else(value)?, &flag)?,
                _ => return Err(format!("Unknown argument {flag}\n{USAGE}").into()),
            }
        }
        Ok(Some(Self {
            node_server: input_file(node_server, "--node-server")?,
            rust_bin: input_file(rust_bin, "--rust-bin")?,
            rounds,
            requests,
            concurrency,
        }))
    }

    fn warmup_requests(&self) -> usize {
        self.concurrency.max(50)
    }
}

fn positive(value: OsString, flag: &str) -> Result<usize> {
    value
        .to_str()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .ok_or_else(|| format!("{flag} requires a positive integer").into())
}

fn input_file(path: Option<PathBuf>, flag: &str) -> Result<PathBuf> {
    let path = path.ok_or_else(|| format!("Missing {flag}\n{USAGE}"))?;
    let path = path
        .canonicalize()
        .map_err(|error| format!("Cannot open {} for {flag}: {error}", path.display()))?;
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()).into());
    }
    Ok(path)
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Gateway {
    Node,
    Rust,
}

impl Gateway {
    fn name(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Rust => "rust",
        }
    }

    fn command(self, options: &Options, address: SocketAddr, data_dir: &Path) -> Command {
        let mut command = match self {
            Self::Node => {
                let mut command = Command::new("node");
                command
                    .arg(&options.node_server)
                    .current_dir(options.node_server.parent().unwrap());
                command
            }
            Self::Rust => {
                let mut command = Command::new(&options.rust_bin);
                command
                    .args(["--host", "127.0.0.1", "--port"])
                    .arg(address.port().to_string())
                    .arg("--no-browser")
                    .current_dir(data_dir);
                command
            }
        };
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", data_dir)
            .env("DATA_DIR", data_dir)
            .env("HOSTNAME", address.ip().to_string())
            .env("PORT", address.port().to_string())
            .env("NODE_ENV", "production")
            .env("DISABLE_BACKGROUND_TOKEN_REFRESH", "1")
            .env("ENABLE_REQUEST_LOGS", "false")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }
}

#[derive(Serialize)]
struct Environment {
    architecture: &'static str,
    kernel_release: String,
    available_parallelism: usize,
    clock_ticks_per_second: u64,
    rss_sample_interval_ms: u128,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    options: Options,
    environment: Environment,
    rounds: Vec<Round>,
    summaries: Summaries,
}

#[derive(Serialize)]
struct Round {
    round: usize,
    gateway: Gateway,
    requests: usize,
    succeeded: usize,
    errors: usize,
    first_error: Option<String>,
    mock_calls: usize,
    cold_ready_ms: f64,
    warmup_requests: usize,
    warmup_mock_calls: usize,
    latency_ms: Option<Percentiles<f64>>,
    measured_seconds: f64,
    successful_requests_per_second: f64,
    warm_rss_kib: u64,
    sampled_peak_rss_kib: u64,
    cpu_ticks: u64,
    gateway_main_thread_cpu_ticks: u64,
    harness_cpu_ticks: u64,
    cpu_ms_per_request: f64,
}

impl Round {
    fn valid(&self) -> bool {
        self.errors == 0 && self.mock_calls == self.requests
    }
}

#[derive(Serialize)]
struct Percentiles<T> {
    p50: T,
    p95: T,
    p99: T,
}

#[derive(Serialize)]
struct Range {
    median: f64,
    min: f64,
    max: f64,
}

impl Range {
    fn of(samples: impl Iterator<Item = f64>) -> Self {
        let mut samples: Vec<_> = samples.collect();
        samples.sort_unstable_by(f64::total_cmp);
        Self {
            median: (samples[(samples.len() - 1) / 2] + samples[samples.len() / 2]) / 2.0,
            min: samples[0],
            max: samples[samples.len() - 1],
        }
    }
}

#[derive(Serialize)]
struct Summary {
    valid_rounds: usize,
    cold_ready_ms: Range,
    latency_ms: Percentiles<Range>,
    successful_requests_per_second: Range,
    warm_rss_kib: Range,
    sampled_peak_rss_kib: Range,
    cpu_ms_per_request: Range,
}

impl Summary {
    fn for_gateway(gateway: Gateway, rounds: &[Round]) -> Option<Self> {
        let rounds = || {
            rounds
                .iter()
                .filter(move |round| round.gateway == gateway && round.valid())
        };
        let valid_rounds = rounds().count();
        if valid_rounds == 0 {
            return None;
        }
        let latencies = || rounds().filter_map(|round| round.latency_ms.as_ref());
        Some(Self {
            valid_rounds,
            cold_ready_ms: Range::of(rounds().map(|round| round.cold_ready_ms)),
            latency_ms: Percentiles {
                p50: Range::of(latencies().map(|value| value.p50)),
                p95: Range::of(latencies().map(|value| value.p95)),
                p99: Range::of(latencies().map(|value| value.p99)),
            },
            successful_requests_per_second: Range::of(
                rounds().map(|round| round.successful_requests_per_second),
            ),
            warm_rss_kib: Range::of(rounds().map(|round| round.warm_rss_kib as f64)),
            sampled_peak_rss_kib: Range::of(
                rounds().map(|round| round.sampled_peak_rss_kib as f64),
            ),
            cpu_ms_per_request: Range::of(rounds().map(|round| round.cpu_ms_per_request)),
        })
    }
}

#[derive(Serialize)]
struct Summaries {
    node: Option<Summary>,
    rust: Option<Summary>,
}

pub async fn run() -> Result<bool> {
    let Some(options) = Options::parse()? else {
        return Ok(true);
    };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let shutdown = async {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    };
    tokio::pin!(shutdown);

    let scratch = tempfile::Builder::new()
        .prefix("9router-benchmark-")
        .tempdir()?;
    let upstream = TcpListener::bind(BIND).await?;
    let fixture = fixture::seed(upstream.local_addr()?);
    let mock_calls = MockCalls::default();
    let mock = mock_calls.router();
    tokio::spawn(async move { axum::serve(upstream, mock).await });
    let environment = Environment {
        architecture: std::env::consts::ARCH,
        kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease")?
            .trim()
            .to_owned(),
        available_parallelism: std::thread::available_parallelism()?.get(),
        clock_ticks_per_second: rustix::param::clock_ticks_per_second(),
        rss_sample_interval_ms: RSS_INTERVAL.as_millis(),
    };
    let benchmark = Benchmark {
        options: &options,
        scratch: &scratch,
        seed: serde_json::to_vec(&fixture)?,
        mock_calls,
        clock_ticks_per_second: environment.clock_ticks_per_second,
    };
    let mut rounds = Vec::new();
    for round in 1..=options.rounds {
        let order = if round % 2 == 1 {
            [Gateway::Node, Gateway::Rust]
        } else {
            [Gateway::Rust, Gateway::Node]
        };
        for gateway in order {
            let mut running = benchmark.launch(gateway, round).await?;
            let result = tokio::select! {
                result = benchmark.measure(&mut running, gateway, round) => result,
                _ = &mut shutdown => Err("Benchmark interrupted".into()),
            };
            let cleanup = running.child.kill().await;
            let result = result.map_err(|error| running.failure(gateway, round, error))?;
            cleanup?;
            eprintln!(
                "Round {round} {} completed {}/{} requests with {} upstream calls",
                gateway.name(),
                result.succeeded,
                result.requests,
                result.mock_calls,
            );
            rounds.push(result);
        }
    }
    let valid = rounds.iter().all(Round::valid);
    let summaries = Summaries {
        node: Summary::for_gateway(Gateway::Node, &rounds),
        rust: Summary::for_gateway(Gateway::Rust, &rounds),
    };
    let report = Report {
        schema_version: 1,
        options,
        environment,
        rounds,
        summaries,
    };
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(valid)
}

struct Benchmark<'a> {
    options: &'a Options,
    scratch: &'a TempDir,
    seed: Vec<u8>,
    mock_calls: MockCalls,
    clock_ticks_per_second: u64,
}

struct Running {
    child: Child,
    address: SocketAddr,
    started: Instant,
    log: PathBuf,
}

impl Running {
    fn failure(&self, gateway: Gateway, round: usize, error: impl std::fmt::Display) -> String {
        let mut message = format!("Round {round} {} failed: {error}", gateway.name());
        if let Ok(mut file) = File::open(&self.log) {
            let mut tail = Vec::new();
            if file
                .metadata()
                .and_then(|metadata| {
                    file.seek(SeekFrom::Start(metadata.len().saturating_sub(4096)))
                })
                .and_then(|_| file.read_to_end(&mut tail))
                .is_ok()
            {
                let tail = String::from_utf8_lossy(&tail);
                if !tail.trim().is_empty() {
                    message.push('\n');
                    message.push_str(tail.trim_end());
                }
            }
        }
        message
    }
}

impl Benchmark<'_> {
    async fn launch(&self, gateway: Gateway, round: usize) -> Result<Running> {
        let data_dir = self
            .scratch
            .path()
            .join(format!("{}-{round}", gateway.name()));
        fs::create_dir(&data_dir)?;
        fs::write(data_dir.join("db.json"), &self.seed)?;
        let log = data_dir.join("gateway.log");
        let output = File::create(&log)?;
        let listener = TcpListener::bind(BIND).await?;
        let address = listener.local_addr()?;
        let mut command = gateway.command(self.options, address, &data_dir);
        command.stdout(output.try_clone()?).stderr(output);
        drop(listener);
        let started = Instant::now();
        let child = command
            .spawn()
            .map_err(|error| format!("Unable to start {}: {error}", gateway.name()))?;
        Ok(Running {
            child,
            address,
            started,
            log,
        })
    }

    async fn measure(
        &self,
        running: &mut Running,
        gateway: Gateway,
        round: usize,
    ) -> Result<Round> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(self.options.concurrency)
            .timeout(Duration::from_secs(10))
            .build()?;
        time::timeout(READY_TIMEOUT, ready(&client, running))
            .await
            .map_err(|_| "Timed out waiting for /api/health")??;
        let cold_ready_ms = milliseconds(running.started.elapsed());
        let label = format!("{}-{round}", gateway.name());
        let warmup_requests = self.options.warmup_requests();
        let calls_before_warmup = self.mock_calls.count();
        let warmup = fixture::send(
            &client,
            running.address,
            &format!("{label}-warmup"),
            warmup_requests,
            self.options.concurrency,
        )
        .await;
        let calls_before_load = self.mock_calls.count();
        let warmup_mock_calls = calls_before_load - calls_before_warmup;
        if warmup.errors != 0 || warmup_mock_calls != warmup_requests {
            let first_error = warmup.first_error.as_deref().unwrap_or("no HTTP error");
            return Err(format!(
                "Warmup had {} errors and {warmup_mock_calls}/{warmup_requests} upstream calls ({first_error})",
                warmup.errors
            )
            .into());
        }

        let pid = running
            .child
            .id()
            .ok_or("Gateway exited before measurement")?;
        let process = Process {
            proc_dir: PathBuf::from(format!("/proc/{pid}")),
        };
        let main_thread = Process {
            proc_dir: process.proc_dir.join("task").join(pid.to_string()),
        };
        let harness = Process {
            proc_dir: PathBuf::from("/proc/self"),
        };
        let warm_rss_kib = process.rss_kib()?;
        let mut sampled_peak_rss_kib = warm_rss_kib;
        let start_ticks = process.cpu_ticks()?;
        let main_thread_start = main_thread.cpu_ticks()?;
        let harness_start = harness.cpu_ticks()?;
        let label = format!("{label}-load");
        let workload = fixture::send(
            &client,
            running.address,
            &label,
            self.options.requests,
            self.options.concurrency,
        );
        tokio::pin!(workload);
        let started = Instant::now();
        let mut samples = time::interval_at(time::Instant::now() + RSS_INTERVAL, RSS_INTERVAL);
        samples.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut requests = loop {
            tokio::select! {
                result = &mut workload => break result,
                _ = samples.tick() => {
                    sampled_peak_rss_kib = sampled_peak_rss_kib.max(process.rss_kib()?);
                }
            }
        };
        let measured_seconds = started.elapsed().as_secs_f64();
        let cpu_ticks = process.cpu_ticks()? - start_ticks;
        let gateway_main_thread_cpu_ticks = main_thread.cpu_ticks()? - main_thread_start;
        let harness_cpu_ticks = harness.cpu_ticks()? - harness_start;
        sampled_peak_rss_kib = sampled_peak_rss_kib.max(process.rss_kib()?);
        let mock_calls = self.mock_calls.count() - calls_before_load;
        if let Some(status) = running.child.try_wait()? {
            return Err(format!("Gateway exited during measurement with {status}").into());
        }
        let succeeded = requests.latencies_ms.len();
        requests.latencies_ms.sort_unstable_by(f64::total_cmp);
        let latency_ms = (succeeded > 0).then(|| Percentiles {
            p50: percentile(&requests.latencies_ms, 0.50),
            p95: percentile(&requests.latencies_ms, 0.95),
            p99: percentile(&requests.latencies_ms, 0.99),
        });
        Ok(Round {
            round,
            gateway,
            requests: self.options.requests,
            succeeded,
            errors: requests.errors,
            first_error: requests.first_error,
            mock_calls,
            cold_ready_ms,
            warmup_requests,
            warmup_mock_calls,
            latency_ms,
            measured_seconds,
            successful_requests_per_second: succeeded as f64 / measured_seconds,
            warm_rss_kib,
            sampled_peak_rss_kib,
            cpu_ticks,
            gateway_main_thread_cpu_ticks,
            harness_cpu_ticks,
            cpu_ms_per_request: 1000.0 * cpu_ticks as f64
                / self.clock_ticks_per_second as f64
                / self.options.requests as f64,
        })
    }
}

async fn ready(client: &Client, running: &mut Running) -> Result<()> {
    let url = format!("http://{}/api/health", running.address);
    loop {
        if let Some(status) = running.child.try_wait()? {
            return Err(format!("Gateway exited during startup with {status}").into());
        }
        if let Ok(response) = client
            .get(&url)
            .timeout(Duration::from_millis(250))
            .send()
            .await
            && response.status() == StatusCode::OK
            && response.bytes().await.is_ok()
        {
            return Ok(());
        }
        time::sleep(READY_POLL).await;
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    sorted[(fraction * sorted.len() as f64).ceil() as usize - 1]
}

struct Process {
    proc_dir: PathBuf,
}

impl Process {
    fn rss_kib(&self) -> Result<u64> {
        let status = fs::read_to_string(self.proc_dir.join("status"))?;
        let rss = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rss| rss.split_whitespace().next())
            .ok_or("Missing VmRSS in /proc/PID/status")?;
        Ok(rss.parse()?)
    }

    fn cpu_ticks(&self) -> Result<u64> {
        let stat = fs::read_to_string(self.proc_dir.join("stat"))?;
        let (_, fields) = stat.rsplit_once(')').ok_or("Malformed /proc/PID/stat")?;
        let mut fields = fields.split_whitespace();
        let user: u64 = fields
            .nth(11)
            .ok_or("Missing utime in /proc/PID/stat")?
            .parse()?;
        let system: u64 = fields
            .next()
            .ok_or("Missing stime in /proc/PID/stat")?
            .parse()?;
        Ok(user + system)
    }
}
