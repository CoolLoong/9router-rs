# Tray replay

## Linux

The upstream tray module uses a simulated systray host. The Rust tray connects to a private D-Bus session with a test StatusNotifier watcher. Replay compares the menu and icon, auto-start toggling under a temporary home, the dashboard command, Quit, removal from D-Bus, and behavior without a desktop session or watcher. A stub records dashboard URLs without opening a browser.

```sh
replay="$PWD/rust/cli-tray-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn sh -c 'rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2"' \
  sh "$replay" "$replay/target/debug/cli-tray-replay"
```

## Windows

The standalone Windows app registers a notification-area icon with Win32. The menu shows the bound port and provides Open Dashboard and Quit. Quit goes through the server's graceful shutdown. Without a desktop, the server continues to accept requests and prints the dashboard URL.

With the Windows GNU Rust target, MinGW, Wine, Xvfb and xauth available, run this from the repository root. The smoke uses a temporary Wine prefix and its own X server. It checks the hidden tray window, status and Quit, plus authenticated HTTP shutdown with the desktop unavailable. It leaves browser navigation uninvoked.

```sh
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --locked --manifest-path rust/app/Cargo.toml --target x86_64-pc-windows-gnu
probe_dir=$(mktemp -d)
x86_64-w64-mingw32-gcc -O2 -municode rust/reference/windows-tray-probe.c -o "$probe_dir/tray-probe.exe"
unshare -Urn -- sh -c 'ip link set lo up && exec python3 "$@"' sh \
  rust/reference/windows-tray-contract.py rust/app/target/x86_64-pc-windows-gnu/debug/server.exe \
  "$(command -v Xvfb)" "$probe_dir/tray-probe.exe"
```
