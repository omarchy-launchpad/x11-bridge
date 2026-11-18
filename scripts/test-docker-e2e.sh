#!/bin/sh
# Build-time downloads are allowed; execution has loopback networking only.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
preview=
gui=
case "${1:-}" in
    '') [ "$#" -eq 0 ] ;;
    --gui)
        if [ "$#" -ne 1 ] || [ "$(uname -s)" != Darwin ]; then echo 'Live GUI preview requires macOS: test-docker-e2e.sh --gui' >&2; exit 2; fi
        gui=1
        ;;
    --preview)
        if [ "$#" -gt 2 ]; then echo 'Usage: test-docker-e2e.sh [--gui | --preview [output.png]]' >&2; exit 2; fi
        preview=${2:-"$repo/target/preview.png"}
        ;;
    *) echo 'Usage: test-docker-e2e.sh [--gui | --preview [output.png]]' >&2; exit 2 ;;
esac
docker build -f "$repo/scripts/Dockerfile.xvfb" -t x11-bridge-xvfb-test "$repo"
if [ -n "$gui" ]; then
    cargo build --manifest-path "$repo/Cargo.toml" -p x11-bridge-protocol --example viewer --locked --target-dir "$repo/target"
    app="$repo/target/X11 Bridge Preview.app"
    mkdir -p "$app/Contents/MacOS"
    cp "$repo/target/debug/examples/viewer" "$app/Contents/MacOS/viewer"
    cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.x11-bridge.preview</string>
<key>CFBundleName</key><string>X11 Bridge Preview</string>
<key>CFBundleExecutable</key><string>viewer</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
    cid_dir=$(mktemp -d "${TMPDIR:-/tmp}/x11-bridge-gui.XXXXXXXX")
    cid="$cid_dir/container"
    cleanup() {
        if [ -f "$cid" ]; then docker rm -f "$(cat "$cid")" >/dev/null 2>&1 || true; fi
        rm -rf "$cid_dir"
    }
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM
    "$app/Contents/MacOS/viewer" docker run --rm -i --network none \
        --cidfile "$cid" --env CARGO_NET_OFFLINE=true --env X11_BRIDGE_GUI=1 x11-bridge-xvfb-test
elif [ -n "$preview" ]; then
    mkdir -p "$(dirname -- "$preview")"
    temporary=$(mktemp "$preview.XXXXXXXX")
    trap 'rm -f "$temporary"' EXIT
    trap 'exit 1' HUP INT TERM
    docker run --rm --network none --env CARGO_NET_OFFLINE=true \
        --env X11_BRIDGE_PREVIEW=1 x11-bridge-xvfb-test > "$temporary"
    mv "$temporary" "$preview"
    printf 'Captured X11 window: %s\n' "$preview"
    if command -v open >/dev/null 2>&1 && [ "$(uname -s)" = Darwin ]; then
        open "$preview"
    elif command -v xdg-open >/dev/null 2>&1; then
        xdg-open "$preview" >/dev/null 2>&1 || true
    fi
else
    docker run --rm --network none \
        --env CARGO_NET_OFFLINE=true x11-bridge-xvfb-test
fi
