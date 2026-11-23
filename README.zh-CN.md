# 9Router Rust

9Router 的 API 网关、网页管理台、CLI 和 Provider 目录已迁移到 Rust。管理台的 HTML、CSS 和浏览器脚本内嵌在二进制里，托管中继模板由程序部署到对应平台。网关通过 Cargo 构建，编译和运行均无需 Node.js 或 npm。

[English](README.md) · [原生打包](docs/native-packages.md)

## 启动

```sh
cargo check --locked --workspace
DATA_DIR="$HOME/.9router-rs" cargo run --locked -- --host 127.0.0.1 --port 20128 --no-browser
```

启动后打开[管理台](http://127.0.0.1:20128/dashboard)。健康检查地址为 `/api/health`，OpenAI 兼容 API 位于 `/v1`。SQLite 数据存放在 `DATA_DIR`。示例使用独立数据目录，接管已有安装前请先备份原数据库。

用 `cargo build --release --locked` 生成 `target/release/ninerouter`，Windows 可执行文件为 `ninerouter.exe`。原生归档中的可执行文件名为 `9router` 或 `9router.exe`。

## Docker

```sh
cp .env.example .env
docker compose up -d --build
```

Compose 将 `9router-data` 挂载到 `/app/data`，同时启动配置中的 Headroom 服务。

## 项目结构

- `src/` 包含网关、管理台、CLI、Provider 和内嵌资源。
- `crates/` 包含内部 Rust 库。
- `tools/package/` 提供原生归档和依赖许可证清单。

基线为 [decolua/9router 的 1855aa4](https://github.com/decolua/9router/tree/1855aa44473e88feac9d3f131b9c0c5fc9706b68)。旧版源码和迁移回放工具保存在 Git 历史中。项目使用 [MIT 许可](LICENSE)。
