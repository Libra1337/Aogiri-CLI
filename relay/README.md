# lynshen-relay

The relay of `docs/relay-protocol.md` §3: forwards opaque Noise traffic between
paired clients and a daemon. It holds no keys, reads no content and stores
nothing on disk.

```sh
go build -o lynshen-relay .
./lynshen-relay -listen 127.0.0.1:18095
```

Put it behind TLS at `https://app.lynshen.org/relay/v1/*` (the daemon default).
Implemented: host auth, client streams, limits of §3.3, `healthz`, and Web
Push (`/relay/v1/push/key`, `/relay/v1/push`) when it has a VAPID key:

```sh
sudo install -d -m 0700 /etc/lynshen-relay
sudo ./lynshen-relay -gen-vapid /etc/lynshen-relay/vapid.json   # prints the public key
# systemd unit: LoadCredential=vapid:/etc/lynshen-relay/vapid.json
```

Not implemented: Getui (the endpoint answers 501). Tests: `go test ./...`
(the RFC 8291 example, VAPID tokens, every refusal, host and client streams).

Interop check against a real daemon:
`cargo run -p lynshen-daemon --example relay_phone -- ws://127.0.0.1:18095/relay/v1 '<pair link>'`
