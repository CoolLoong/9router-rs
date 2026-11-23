# 9Router Rust

9Router's API gateway, web dashboard, CLI, and provider registry in Rust. The server embeds the dashboard's HTML, CSS, and browser scripts. Managed relay templates are deployed to the selected relay platform. Cargo builds the gateway without Node.js or npm. The resulting binary runs standalone.

[简体中文](README.zh-CN.md) · [Native packages](docs/native-packages.md)

## Run locally

```sh
cargo check --locked --workspace
DATA_DIR="$HOME/.9router-rs" cargo run --locked -- --host 127.0.0.1 --port 20128 --no-browser
```

Open [the dashboard](http://127.0.0.1:20128/dashboard). The health endpoint is `/api/health`, and the OpenAI-compatible API is under `/v1`. The gateway stores SQLite data in `DATA_DIR`. The example uses a separate directory for a fresh installation. Keep an existing database backup before pointing the gateway at a previous installation.

Build with `cargo build --release --locked`. The executable is `target/release/ninerouter` (`ninerouter.exe` on Windows). The native packager archives it as `9router` or `9router.exe`.

## Docker

```sh
cp .env.example .env
docker compose up -d --build
```

Compose mounts `9router-data` at `/app/data` and starts the Headroom service defined in the compose file.

## Layout

- `src/` contains the gateway, dashboard, CLI, providers, and embedded assets.
- `crates/` contains the internal Rust libraries.
- `tools/package/` contains the native archive and license-notice packager.

The upstream baseline is [decolua/9router at 1855aa4](https://github.com/decolua/9router/tree/1855aa44473e88feac9d3f131b9c0c5fc9706b68). The legacy source and migration replay tools remain available in this repository's Git history. The project is licensed under [MIT](LICENSE).
