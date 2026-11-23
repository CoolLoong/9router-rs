#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
scratch=$(mktemp -d -t 9router-cli-tools.XXXXXXXX)
mkdir -p "$scratch/home" "$scratch/config" "$scratch/bin"
for command in dirname env mktemp node python3 sh; do
    ln -s "$(command -v "$command")" "$scratch/bin/$command"
done
cat > "$scratch/bin/which" <<'WHICH'
#!/bin/sh
command -v "$1"
WHICH
chmod +x "$scratch/bin/which"
if [ "$1" = "reference" ]; then
    python3 "$root/rust/reference/seed.py" "$scratch/data"
    cd "$root"
    exec env -i PATH="$scratch/bin" HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/config"         DATA_DIR="$scratch/data" PORT="$PORT" HOSTNAME=127.0.0.1 NEXT_TELEMETRY_DISABLED=1         "$scratch/bin/node" "$root/.next/standalone/custom-server.js"
fi
exec env -u JWT_SECRET -u INITIAL_PASSWORD -u AUTH_COOKIE_SECURE     PATH="$scratch/bin" HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/config"     DATA_DIR="$scratch/data" NINEROUTER_MIGRATION_FIXTURE=1 "$root/rust/app/target/debug/server"
