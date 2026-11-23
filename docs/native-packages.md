# Native CLI preview packages

The Cargo packager builds the application in release mode and writes a tar.gz archive on Linux and macOS or a ZIP on Windows. Cargo needs the locked crates, the selected Rust target, and its linker. Build the [dashboard assets](dashboard-assets.md) from source before packaging. Cargo's packaging build runs offline.

```sh
cargo run --offline --locked -p ninerouter-package --
cargo run --offline --locked -p ninerouter-package -- --target aarch64-apple-darwin --output /tmp/9router-dist
```

The default output is `dist/`. Use `--target-dir` to select a Cargo build directory. The archive contains the `9router` executable, its version and SHA-256 in `manifest.json`, and project, asset, and dependency licenses. Static musl packages include the Rust toolchain notice from `share/doc/rust/COPYRIGHT-library.html`. Install the `rust-docs` component for that toolchain if the notice is missing.

## Install on Arch Linux with paru

From the repository root on x86_64, run:

```sh
paru -Bi packaging/arch
```

The recipe fetches the `master` branch from GitHub, builds the dashboard and Rust executable, and installs `9router-rs-git`. Pacman owns `/usr/bin/9router`. Run the same command to build and install a newer commit.

## Linux preview updates

Set `NINEROUTER_NATIVE_UPDATE_MANIFEST` to the HTTPS URL of the release manifest. Set `NINEROUTER_NATIVE_UPDATE_VERIFY_KEY` to the unpadded Base64URL encoding of the trusted 32-byte Ed25519 public key. Both settings are read by the running process. The preview has no built-in release feed or key.

The outer JSON document contains `payload` and `signature` as unpadded Base64URL strings. The signature covers the exact decoded `payload` bytes. That JSON payload contains the following fields.

| Field | Value |
| --- | --- |
| `schema` | `9router-native-update/v1` |
| `channel` | `preview` |
| `target` | The Cargo target triple for the running binary |
| `version` | A newer semantic CLI version |
| `expiresAt` | Expiration as Unix seconds |
| `artifact` | HTTPS URL or relative path on the same origin as the manifest |
| `size` | Byte length of the raw native executable |
| `sha256` | Lowercase hex SHA-256 of the raw executable |

The dashboard version API reads the signed feed. `POST /api/version/update` requires an authenticated dashboard session and stages the verified executable next to the running `9router`. It then drains the gateway, swaps the executable atomically and checks that the new process owns the listener and reports the expected version and health. Launch failure restores the previous executable and restarts it. Keep the releases compatible with the same data directory so either executable can read it.

If the launcher is interrupted after the swap, the private `.9router-update-*` directory retains `previous`. This file is the pre-update executable. Restore it with the gateway stopped. Use a user-owned, writable installation directory for this preview path.

## Windows preview updates

The Windows package uses the same HTTPS feed and Ed25519 public-key settings. Use the Windows Cargo target and sign metadata for the raw `9router.exe` artifact. The distribution ZIP remains the manually installed preview package.

The launcher reports a newer verified preview and `GET /api/version` exposes the verified version. `--skip-update` suppresses the startup check.

`POST /api/version/update` requires an authenticated dashboard session. It downloads and verifies the raw PE before shutting down the gateway. The updater requires a writable, private installation named `9router.exe`, an IPv4 listener and the same persistent data directory throughout the update. It holds an exclusive lease for each of the installation and store.

After the original gateway exits, the helper records the previous executable, SQLite database and any WAL in the private `.9router-update` directory before replacing the executable. It accepts the replacement only after checking the owned listener, health and running version. If the helper or rollback is interrupted, restarting the same installation with the original data directory detects the journal before opening SQLite and repeats recovery. Recoverable files stay in the update directory until a later update clears the completed transaction.
