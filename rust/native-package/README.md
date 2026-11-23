# Native CLI preview packages

`pack.py` builds the pinned CLI version with Cargo in release mode and archives it as `9router` on Linux and macOS or `9router.exe` on Windows. Use Python 3.12 or newer for building and smoke testing. Cargo needs the locked crates, the chosen Rust target, and its linker. The build runs offline.

```sh
python3 rust/native-package/pack.py
python3 rust/native-package/pack.py --target aarch64-apple-darwin --output /tmp/9router-dist
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  python3 rust/native-package/pack.py --target x86_64-pc-windows-gnu
```

The default output is `rust/native-package/dist`. `--target-dir` selects an existing Cargo build directory. `manifest.json` records the CLI version, target and binary SHA-256. The archive contains both project MIT notices, embedded asset licenses and dependency notices. The license supplements record their original source and the corresponding crate version. Static Linux musl archives also include `licenses/toolchain/COPYRIGHT-library.html`, copied from `share/doc/rust/` in the active Rust sysroot. Install `rust-docs` for that toolchain to build the archives.

The archive uses `native-package` to exclude the legacy launcher. Migrated routes and CLI commands run with the native executable. Routes still awaiting migration return HTTP 503 with a legacy-runtime error. The default debug `server` build keeps its reference fallback for `rust-anything` replay. Archives are unsigned previews.

```sh
archive=/absolute/path/to/9router-native-preview.tar.gz
unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh rust/native-package/smoke.py "$archive"
```

On a Linux host with QEMU and an ARM64 GNU sysroot, append `--runner /usr/bin/qemu-aarch64-static -L /usr/aarch64-linux-gnu` to smoke-test the ARM64 archive.

For static musl builds using Zig, point Cargo's target linker and the `CC_<target>` variable at a wrapper for `zig cc -target x86_64-linux-musl` or `zig cc -target aarch64-linux-musl`. Strip the incoming `--target=<Rust target>` argument in the wrapper so Zig receives a single target. Set the corresponding `CARGO_TARGET_<TARGET>_RUSTFLAGS` to `-C link-self-contained=no` so Zig supplies the startup objects once. The static ARM64 archive smoke test only needs `--runner /usr/bin/qemu-aarch64-static`.

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

The fixture generates an ephemeral signing key and stages both versions in temporary directories. Run it with two Linux binaries built with `--features native-package` and distinct CLI versions.

```sh
sh rust/native-package/update-fixture.sh /absolute/path/to/current/server /absolute/path/to/newer/server
```

## Windows preview updates

The Windows package uses the same HTTPS feed and Ed25519 public-key settings. Use the Windows Cargo target and sign metadata for the raw `9router.exe` artifact. The distribution ZIP remains the manually installed preview package.

The launcher reports a newer verified preview and `GET /api/version` exposes the verified version. `--skip-update` suppresses the startup check.

`POST /api/version/update` requires an authenticated dashboard session. It downloads and verifies the raw PE before shutting down the gateway. The updater requires a writable, private installation named `9router.exe`, an IPv4 listener and the same persistent data directory throughout the update. It holds an exclusive lease for each of the installation and store.

After the original gateway exits, the helper records the previous executable, SQLite database and any WAL in the private `.9router-update` directory before replacing the executable. It accepts the replacement only after checking the owned listener, health and running version. If the helper or rollback is interrupted, restarting the same installation with the original data directory detects the journal before opening SQLite and repeats recovery. Recoverable files stay in the update directory until a later update clears the completed transaction.

```sh
unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh \
  rust/native-package/windows-update-discovery-fixture.py /absolute/path/to/server.exe
```

The Windows process fixture uses a fresh Wine prefix and builds temporary PE files with MinGW. It verifies exclusive install locking, direct duplication of non-inheritable handles into the suspended child, and a read-only handoff tied to that child. It waits for parent exit, verifies the child's listener and tests rollback after a failed boot, wrong version, or occupied listener. Directory checks reject world-writable, reparse, out-of-installation and changed-identity paths. The signed-in Windows account, SYSTEM and administrators are trusted. Other principals may have read access to the installation but must lack write access.

Both Win32 relative-rename classes returned error 87 under Wine in this fixture. The equivalent native information classes successfully replaced temporary files using the validated destination directory handle. [Windows file rename information](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information)

```sh
unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh \
  rust/native-package/windows-update-process-fixture.py
```

Pass two Windows `native-package` binaries built from the same source using CLI versions 0.5.119 and 0.5.120 to exercise the authenticated production route against a loopback feed. The fixture checks upgrade, digest mismatch, version-mismatch rollback and automatic recovery after interruption during both executable and SQLite restoration. It uses a fresh Wine prefix and checks the running version and persisted SQLite values.

```sh
unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh \
  rust/native-package/windows-native-update-fixture.py /absolute/path/to/previous.exe /absolute/path/to/newer.exe
```
