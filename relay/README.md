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

APNs and FCM for the Aogiri app (relay-protocol.md, "Native push"), each
only with its credentials (root-only files, passed in by systemd):

```sh
sudo install -m 0600 AuthKey_<KEYID>.p8 /etc/lynshen-relay/apns.p8
sudo install -m 0600 <firebase-service-account>.json /etc/lynshen-relay/fcm.json
# systemd unit: LoadCredential=apns:/etc/lynshen-relay/apns.p8
#               LoadCredential=fcm:/etc/lynshen-relay/fcm.json
#               Environment=LYNSHEN_RELAY_APNS_KEY_ID=<KEYID> LYNSHEN_RELAY_APNS_TEAM_ID=<TEAMID>
```

Without them those providers answer 501; Web Push is unaffected. Not
implemented: Getui (501). Tests: `go test ./...` (the RFC 8291 example, VAPID
and APNs/FCM tokens, every refusal, provider answers, host and client streams).

Relay v2 (account-based, `docs/relay-protocol.md` §7) is served next to v1
when the relay knows the gateway:

```sh
# systemd unit: LoadCredential=relay-service:/etc/lynshen-relay/relay-service
#               Environment=LYNSHEN_RELAY_GATEWAY=https://www.lynshen.org
```

The relay checks relay tokens offline with the gateway's key
(`/api/desktop/oauth/relay-key`). It registers computers and sends a heartbeat
every 60 s with the service token. `-no-v1` turns v1 off; keep v1 on while
phones from before v2 are in use. Tests: `go test -race ./...`.

Interop check against a real daemon:
`cargo run -p lynshen-daemon --example relay_phone -- ws://127.0.0.1:18095/relay/v1 '<pair link>'`
