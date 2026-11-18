#!/bin/sh
# Internal Docker runner: authenticated Xvfb and SSH in one disposable workspace.
set -eu
umask 077
work=$(mktemp -d /home/bridge/e2e.XXXXXXXX)
xserver=
ssh_server=
cleanup() {
    for pid in "$ssh_server" "$xserver"; do
        if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

export XAUTHORITY="$work/authority"
touch "$XAUTHORITY"
cookie=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
# -displayfd chooses a free display; Xauthority's dummy entry supplies the
# cookie to Xvfb before the display number is known.
xauth -f "$XAUTHORITY" add :65535 . "$cookie"
Xvfb -displayfd 3 -screen 0 1024x768x24 -nolisten tcp -auth "$XAUTHORITY" 3>"$work/display" >"$work/server.log" 2>&1 &
xserver=$!
attempt=0
while [ ! -s "$work/display" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -gt 100 ] || ! kill -0 "$xserver" 2>/dev/null; then cat "$work/server.log" >&2; exit 1; fi
    sleep 0.1
done
export X11_BRIDGE_DISPLAY=":$(cat "$work/display")"
xauth -f "$XAUTHORITY" add "$X11_BRIDGE_DISPLAY" . "$cookie"
if [ "${X11_BRIDGE_GUI:-0}" = 1 ]; then
    target/debug/examples/desktop_demo
    exit 0
fi
if [ "${X11_BRIDGE_PREVIEW:-0}" = 1 ]; then
    X11_BRIDGE_TEST_INPUT=0 timeout 45 target/debug/examples/stream_smoke --preview \
        target/debug/examples/streaming_fixture
    exit 0
fi
export X11_BRIDGE_TEST_INPUT=1
cargo test --workspace --locked
cargo test -p x11-bridge-agent --test x11_live --locked -- --ignored --test-threads=1 --nocapture

cargo build --workspace --bins --examples --locked
# Unlock only this disposable account. Password authentication stays disabled.
password=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
printf 'bridge:%s\n' "$password" | chpasswd
unset password
ssh-keygen -q -t ed25519 -N '' -f "$work/host_key"
ssh-keygen -q -t ed25519 -N '' -f "$work/client_key"
ssh-keygen -q -t ed25519 -N '' -f "$work/rejected_key"
cp "$work/client_key.pub" "$work/authorized_keys"
cp "$XAUTHORITY" "$work/xauthority"
chown bridge:bridge "$work" "$work/authorized_keys" "$work/xauthority"
printf '[127.0.0.1]:2222 ' > "$work/known_hosts"
cut -d ' ' -f 1,2 "$work/host_key.pub" >> "$work/known_hosts"
mkdir -p /run/sshd
cat > "$work/sshd_config" <<CONFIG
ListenAddress 127.0.0.1
Port 2222
HostKey $work/host_key
PidFile $work/sshd.pid
AuthorizedKeysFile $work/authorized_keys
StrictModes yes
AllowUsers bridge
PermitRootLogin no
AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowTcpForwarding no
AllowAgentForwarding no
X11Forwarding no
PermitTTY no
ForceCommand /work/target/debug/examples/streaming_fixture
SetEnv X11_BRIDGE_DISPLAY=$X11_BRIDGE_DISPLAY XAUTHORITY=$work/xauthority
AcceptEnv X11_BRIDGE_TEST_INPUT
CONFIG
/usr/sbin/sshd -t -f "$work/sshd_config"
/usr/sbin/sshd -D -e -f "$work/sshd_config" > "$work/sshd.log" 2>&1 &
ssh_server=$!
attempt=0
# This scan checks readiness only. Trust is pinned to the generated public key.
until ssh-keyscan -T 1 -p 2222 127.0.0.1 > /dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ "$attempt" -gt 20 ] || ! kill -0 "$ssh_server" 2>/dev/null; then cat "$work/sshd.log" >&2; exit 1; fi
    sleep 0.1
done

# Assert that an unauthorised client cannot enter the fixture account.
status=0
ssh -F /dev/null -T -p 2222 -i "$work/rejected_key" \
    -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
    -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$work/known_hosts" \
    -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=5 \
    bridge@127.0.0.1 </dev/null > /dev/null 2> "$work/rejected.log" || status=$?
if [ "$status" -ne 255 ] || ! grep -q 'Permission denied (publickey)' "$work/rejected.log"; then
    echo 'Unauthorised SSH client was not rejected as expected' >&2
    exit 1
fi

# Confirm the X server requires its cookie, even for a local fixture process.
status=0
XAUTHORITY="$work/missing-authority" X11_BRIDGE_TEST_INPUT=0 \
    timeout 5 target/debug/examples/streaming_fixture </dev/null > /dev/null 2>&1 || status=$?
if [ "$status" -ne 1 ]; then
    echo 'Unauthenticated X11 fixture was not rejected as expected' >&2
    exit 1
fi

for input in 0 1; do
    echo "Docker SSH/X11 end-to-end test: input=$input"
    X11_BRIDGE_TEST_INPUT=$input timeout 45 target/debug/examples/stream_smoke \
        ssh -F /dev/null -T -p 2222 -i "$work/client_key" \
        -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
        -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$work/known_hosts" \
        -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
        -o SendEnv=X11_BRIDGE_TEST_INPUT bridge@127.0.0.1 || {
            cat "$work/sshd.log" >&2
            exit 1
        }
done
echo 'Docker SSH/X11 end-to-end tests passed; authentication failures rejected.'
