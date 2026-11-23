# 9Router gateway benchmark

Compare original 9Router and 9router-rs on Linux using the authenticated, non-streaming `POST /v1/chat/completions` path. Supply the original standalone package's `app/server.js`, a compatible `node` on `PATH`, and the native Rust release executable.

Fetch the reference package into a temporary directory.

```sh
reference=$(mktemp -d)
npm pack --ignore-scripts --pack-destination "$reference" 9router@0.5.119
tar -xzf "$reference/9router-0.5.119.tgz" -C "$reference"
```

Use `$reference/package/app/server.js` as the Node entry point. Build the Rust executable from [Quick Start](../../README.md#-quick-start) or extract it from a native package.

Build from the workspace root, then run in a network namespace with loopback enabled. The namespace command requires `unshare`, `ip`, and support for unprivileged user namespaces.

```sh
cargo build --locked --release -p nine-router-benchmark

unshare -Urn sh -c 'ip link set lo up && exec "$@"' benchmark \
  target/release/nine-router-benchmark \
  --node-server /path/to/original-package/app/server.js \
  --rust-bin /path/to/native-package/9router \
  --rounds 5 --requests 1000 --concurrency 8 > benchmark.json
```

The command shows the defaults. Each round starts a fresh gateway process for each implementation and alternates which goes first. Both receive the same synthetic `db.json` in separate temporary directories and forward requests to the same Axum echo server on loopback. The benchmark uses fixture keys and ephemeral ports, clears the child environment apart from `PATH` and its own settings, and disables background token refresh. Each process must pass `/api/health` and at least 50 warmup requests before the measured load begins.

Each request has a unique input. A successful response has HTTP 200 and the matching echo in the assistant message. The warmup and measurement each require one mock call per request. A failed startup or warmup exits with diagnostics on stderr. A measured request error or mock-count mismatch exits nonzero and leaves the completed measurements in the JSON report.

## Report

`rounds` contains one record per process in execution order. `summaries.node` and `summaries.rust` contain the median, minimum, and maximum across valid rounds for each metric. A summary is `null` when it has zero valid rounds.

| Metric | Scope |
| --- | --- |
| `cold_ready_ms` | Process spawn to first complete HTTP 200 from `/api/health`, polled every 10 ms. Each run starts a fresh process with normal OS caches. |
| `latency_ms` | Client send to full response body for successful requests, using nearest-rank p50, p95, and p99. |
| `successful_requests_per_second` | Successful responses divided by the elapsed wall time of the concurrent measured workload. |
| `warm_rss_kib` | Linux `VmRSS` for the gateway process immediately after warmup. |
| `sampled_peak_rss_kib` | Maximum gateway `VmRSS` at the warm baseline, during 20 ms sampling, and at the end of the workload. |
| `cpu_ms_per_request` | Gateway process user plus system CPU time during the load window divided by attempted requests. Linux CPU tick granularity affects short runs. |
| `gateway_main_thread_cpu_ticks` | CPU ticks for the gateway's main thread during the measured load. |
| `harness_cpu_ticks` | CPU ticks for the benchmark process, which runs both the client and the mock. |

Linux process metrics cover the direct gateway PID and its threads. The Node PID runs `node app/server.js` directly, so the reference measurement excludes launcher overhead. Both workloads share the host with the benchmark client and Axum mock. Results describe the selected packages, machine, and local echo workload.

## Recorded run

The [Linux x86_64 report](results/linux-x86_64-0.5.119.json) records five valid rounds per gateway on an Intel Core i7-14700HX. During the concurrency-8 runs, the Node main thread used a median of 0.96 CPU cores. In the single concurrency-16 check, Node throughput stayed inside the concurrency-8 run range. The benchmark process, including the client and mock, used below 0.10 cores at the median on both sides.

A separate Rust diagnostic run used `strace -f -k -e trace=clock_nanosleep`. It showed SQLite busy-handler backoff during concurrent requests. The timing table uses the five runs without tracing. The trace establishes that this wait occurs in the workload. It leaves its share of the tail latency undetermined.
