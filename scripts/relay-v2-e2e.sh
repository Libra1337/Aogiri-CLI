#!/bin/bash
# B14 relay v2 end to end: local gateway (test build, throwaway PostgreSQL),
# relay v2 (Go), the daemon (signed in with a CLI device key in a temp HOME),
# and a fake phone (account_phone example). Prints PASS/FAIL lines only; no
# keys or tokens.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
set -u
# MONOIZE_REPO: a checkout of the gateway (monoizeovo) with
# examples/relay_e2e_gateway.rs; PG_BIN: a PostgreSQL bin directory (initdb,
# pg_ctl, postgres). Run from this repository.
GW=${MONOIZE_REPO:?set MONOIZE_REPO to the gateway checkout}
CLI=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d /tmp/b14-e2e.XXXXXX)
PGBIN=${PG_BIN:?set PG_BIN to a PostgreSQL bin directory}
PIDS=()
cleanup() {
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do kill "$pid" 2>/dev/null; done
  sleep 1
  "$PGBIN/pg_ctl" -D "$WORK/pg" -m immediate stop >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== build"
(cd "$GW" && cargo build --locked --example relay_e2e_gateway 2>&1 | grep -E '^(error|warning: unused)' | head -20) || exit 1
(cd "$CLI" && cargo build -p lynshen-cli -p lynshen-daemon --examples --bins 2>&1 | grep -E '^error' | head -20)
(cd "$CLI/relay" && go build -o "$WORK/lynshen-relay" .) || exit 1

echo "== postgres"
"$PGBIN/initdb" -D "$WORK/pg" -U postgres -A trust >/dev/null || exit 1
PGPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
"$PGBIN/pg_ctl" -D "$WORK/pg" -o "-p $PGPORT -k $WORK -c listen_addresses=127.0.0.1" -l "$WORK/pg.log" start >/dev/null || exit 1
sleep 2
"$PGBIN/postgres" --version >/dev/null
python3 - "$PGPORT" <<'PY' || exit 1
import socket, struct, sys
# CREATE DATABASE over the wire protocol (this PG build ships no psql/createdb).
port = int(sys.argv[1])
s = socket.create_connection(("127.0.0.1", port))
params = b"user\0postgres\0database\0postgres\0\0"
s.send(struct.pack("!ii", 8 + len(params), 196608) + params)
def read():
    head = s.recv(5)
    kind, length = head[:1], struct.unpack("!i", head[1:5])[0]
    body = b""
    while len(body) < length - 4:
        body += s.recv(length - 4 - len(body))
    return kind, body
while read()[0] != b"Z":
    pass
q = b"CREATE DATABASE monoize_e2e\0"
s.send(b"Q" + struct.pack("!i", 4 + len(q)) + q)
while True:
    kind, body = read()
    if kind == b"E":
        sys.exit("create database failed")
    if kind == b"Z":
        break
PY

SEED=$(python3 -c 'import os,base64;print(base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode())')
SERVICE=$(python3 -c 'import os,base64;print(base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode())')
GWPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
RELAYPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
DAEMONPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')

echo "== gateway"
MONOIZE_DATABASE_DSN="postgres://postgres@127.0.0.1:$PGPORT/monoize_e2e" \
MONOIZE_LISTEN="127.0.0.1:$GWPORT" MONOIZE_RELAY_SIGNING_KEY="$SEED" MONOIZE_RELAY_SERVICE_TOKEN="$SERVICE" \
MONOIZE_DEVICE_HWID_MODE=record RUST_LOG=warn \
  "$CARGO_TARGET_DIR/debug/examples/relay_e2e_gateway" "$WORK/keys" > "$WORK/gateway.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 120); do [ -f "$WORK/keys/ready" ] && break; sleep 1; done
[ -f "$WORK/keys/ready" ] || { echo "FAIL gateway did not start"; tail -5 "$WORK/gateway.log" | cut -c1-300; exit 1; }
echo "PASS gateway up, accounts and device keys seeded through the device flow"

echo "== relay v2 (v1 still served)"
LYNSHEN_RELAY_SERVICE_TOKEN="$SERVICE" "$WORK/lynshen-relay" -listen "127.0.0.1:$RELAYPORT" \
  -gateway "http://127.0.0.1:$GWPORT" > "$WORK/relay.log" 2>&1 &
PIDS+=($!)
sleep 1
curl -sf "http://127.0.0.1:$RELAYPORT/relay/v2/healthz" >/dev/null && curl -sf "http://127.0.0.1:$RELAYPORT/relay/v1/healthz" >/dev/null \
  && echo "PASS relay serves v1 and v2" || { echo "FAIL relay"; exit 1; }

echo "== daemon (temp HOME, signed in with the CLI device key)"
export HOME="$WORK/home"
mkdir -p "$HOME/.lynshen"
python3 - "$WORK/keys/computer.key" "$HOME/.lynshen" "$GWPORT" <<'PY'
import json, sys, os
key = open(sys.argv[1]).read().strip()
d = sys.argv[2]
json.dump({"provider": "fake", "protocol": "chat", "model": "fake", "base_url": "http://127.0.0.1:9",
           "lynshen_web_url": "http://127.0.0.1:%s" % sys.argv[3], "lynshen_api_url": "http://127.0.0.1:%s" % sys.argv[3]},
          open(os.path.join(d, "config.json"), "w"))
fd = os.open(os.path.join(d, "auth.json"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
os.write(fd, json.dumps({"providers": {"lynshen-cli": key}}).encode())
os.close(fd)
PY
LYNSHEN_DEVICE_ID=off "$CARGO_TARGET_DIR/debug/lynshen" daemon --listen "127.0.0.1:$DAEMONPORT" \
  --relay "ws://127.0.0.1:$RELAYPORT/relay/v1" > "$WORK/daemon.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 30); do [ -f "$HOME/.lynshen/daemon/token" ] && break; sleep 0.5; done
# The desktop turns remote access on (relay_set), as Settings does.
"$CARGO_TARGET_DIR/debug/lynshen" daemon relay on --listen "127.0.0.1:$DAEMONPORT" | sed 's/^/   /'
sleep 4

echo "== fake phone"
LYNSHEN_DEVICE_ID=off "$CARGO_TARGET_DIR/debug/examples/account_phone" "http://127.0.0.1:$GWPORT" \
  "ws://127.0.0.1:$RELAYPORT/relay/v2" "127.0.0.1:$DAEMONPORT" "$HOME/.lynshen/daemon/token" \
  "$WORK/keys/phone.key" "$WORK/keys/other.key" "$WORK/keys/computer.key"
STATUS=$?
[ $STATUS -eq 0 ] || exit $STATUS

echo "== the computer is removed from the account → it signs itself out, back on v1"
OWNKEY=$(cat "$WORK/keys/computer.key")
DEVS=$(curl -s -H "Authorization: Bearer $OWNKEY" "http://127.0.0.1:$GWPORT/api/desktop/oauth/devices")
OWNID=$(python3 -c 'import json,sys;d=json.loads(sys.argv[1]);print(next(x["id"] for x in d["devices"] if x["api_key_id"]==d["current_api_key_id"]))' "$DEVS")
# The computer's own row is removed by the admin path of the store (as the console would).
python3 - "$PGPORT" "$OWNID" <<'PY' || exit 1
import socket, struct, sys
port, dev = int(sys.argv[1]), sys.argv[2]
s = socket.create_connection(("127.0.0.1", port))
params = b"user\0postgres\0database\0monoize_e2e\0\0"
s.send(struct.pack("!ii", 8 + len(params), 196608) + params)
def read():
    head = s.recv(5)
    kind, length = head[:1], struct.unpack("!i", head[1:5])[0]
    body = b""
    while len(body) < length - 4:
        body += s.recv(length - 4 - len(body))
    return kind, body
while read()[0] != b"Z":
    pass
q = ("DELETE FROM api_keys WHERE id = (SELECT api_key_id FROM authorized_devices WHERE id = '%s')\0" % dev).encode()
s.send(b"Q" + struct.pack("!i", 4 + len(q)) + q)
while True:
    kind, body = read()
    if kind == b"E":
        sys.exit("delete failed")
    if kind == b"Z":
        break
PY
# Key caches in the gateway last up to 60 s; the relay drops the host at its next heartbeat.
for _ in $(seq 1 30); do
  python3 -c 'import json,sys;sys.exit(0 if "lynshen-cli" not in json.load(open(sys.argv[1]))["providers"] else 1)' "$HOME/.lynshen/auth.json" && break
  sleep 5
done
python3 -c 'import json,sys;sys.exit(0 if "lynshen-cli" not in json.load(open(sys.argv[1]))["providers"] else 1)' "$HOME/.lynshen/auth.json" \
  && echo "PASS the removed computer signed itself out (device key gone from auth.json)" || { echo "FAIL the computer is still signed in"; exit 1; }
sleep 3
"$CARGO_TARGET_DIR/debug/lynshen" daemon relay status --listen "127.0.0.1:$DAEMONPORT" | sed 's/^/   /'
grep -q 'revoked by the account' "$WORK/relay.log" && echo "PASS the relay dropped the revoked host (4403)" || echo "INFO the relay saw the host leave on its own"


grep -c 'connected (daemon' "$WORK/relay.log" | sed 's/^/   relay host connections: /'
grep -E 'revoked|registration failed' "$WORK/relay.log" | head -3
exit $STATUS
