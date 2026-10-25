# Relay protocol (v1)

Lets a paired phone (the PWA at `https://app.lynshen.org`) reach a desktop's
`lynshen daemon` from outside the LAN. Three parties:

- **host**: the daemon. Keeps one outbound WebSocket to the relay.
- **relay**: `lynshen-relay` (Go, this repo's `relay/`). Forwards
  opaque bytes between a client and its host. Holds no keys, sees no content,
  stores nothing on disk.
- **client**: the PWA (or any device). Opens one WebSocket per session to the
  relay, naming the host.

End-to-end encryption: every client stream carries a Noise
`Noise_IK_25519_ChaChaPoly_SHA256` session between the client and the daemon.
The relay only ever sees Noise handshake and transport messages.

All base64 in this document is **base64url without padding**.

## 1. Keys and identities

The daemon keeps two keypairs in `<daemon state dir>/relay-identity.json`
(mode 0600), created on first use:

| Key | Algorithm | Use |
| --- | --- | --- |
| identity key | Ed25519 | authenticates the host to the relay |
| static key | X25519 | the Noise responder static key (`s` in IK) |

**Host id** = first 16 bytes of `SHA-256(ed25519 public key)`, base64url →
22 characters. It names the host at the relay and in pairing links.

Each client keeps its own X25519 static keypair (the PWA: localStorage key
`lynshen-relay-device`, JSON `{priv, pub}`, base64url). A paired device is
identified to the daemon by its static public key.

## 2. Pairing link

Desktop shows a QR code / link:

```
https://<relay host>/remote#pair=<host_id>.<host_static_pub>.<code>
```

The link is on the relay's own origin (`--relay wss://host/relay/v1` →
`https://host/remote`; `app.lynshen.org` by default).

- `host_static_pub`: the daemon's X25519 public key (32 bytes, base64url).
- `code`: a one-time pairing code from the existing `pair_start` op (8 chars,
  5 minutes, single use).
- The fragment is never sent to any server. The PWA reads it, stores
  `{host_id, host_static_pub, relay: "wss://<link origin>/relay/v1"}` under
  localStorage `lynshen-relay-host`, then clears the fragment
  (`history.replaceState`).

## 3. Relay endpoints

Base: `wss://app.lynshen.org/relay/v1` (Caddy terminates TLS and proxies to the
relay on `127.0.0.1:18095`; the relay itself speaks plain WS/HTTP).

### 3.1 Host connection: `GET /relay/v1/host`

After the WebSocket upgrade:

1. relay → host, text: `{"t":"challenge","nonce":"<32 random bytes, b64>"}`
2. host → relay, text:
   `{"t":"auth","pub":"<ed25519 pub, b64>","sig":"<sig over \"lynshen-relay-v1:\" || nonce bytes, b64>","v":"<daemon version>"}`
3. relay verifies the signature, derives the host id from `pub`, and replies
   `{"t":"ready","host":"<host_id>"}`. A newer authenticated connection for the
   same host id replaces the older one (the older one is closed with code 4409).
   Failure: close code 4401.

Then all messages are **binary** frames:

```
byte 0      kind: 1 = open, 2 = data, 3 = close
bytes 1..4  stream id, u32 big-endian (allocated by the relay, never 0)
bytes 5..   payload (data only)
```

- relay → host `open sid`: a client connected. Payload empty.
- `data sid payload`: both directions; payload is one client message.
- `close sid`: both directions; the other side of the stream closes. The host
  sends it to drop a client (bad handshake, revoked device).

Keepalive: the relay pings the host every 30 s (WebSocket ping); the host
reconnects when its socket drops, with backoff 1 s, 2 s, 4 s … capped at 60 s,
plus jitter.

### 3.2 Client connection: `GET /relay/v1/connect?host=<host_id>`

- Host not connected → the relay accepts the upgrade and immediately closes
  with code **4404** (reason `host offline`), so browsers can read the code.
- Otherwise the relay allocates a stream id, sends `open` to the host, and then
  forwards: each **binary** client message ⇄ one `data` frame. Text frames from
  the client are a protocol error (close 4400).
- Either side closing ends the stream (`close` to the host / WS close 4410
  `host closed` to the client). Host disconnect closes all its client streams
  with 4404.

### 3.3 Limits (relay)

| Limit | Value |
| --- | --- |
| max message (client or data payload) | 1 MiB |
| streams per host | 32 (further connects close 4429) |
| new client connections per IP | 20 per minute (close 4429) |
| host auth attempts per IP | 10 per minute |
| handshake (challenge answered) timeout | 10 s |
| idle client stream (no traffic either way) | 10 min |

Terminal output is flow-controlled end to end by the daemon
(daemon-protocol.md → Terminals: `term_ack`, at most 512 KiB in flight).

`GET /relay/v1/healthz` → `200 ok`.
Logs: connects/disconnects with host id and stream counts; never payloads.

### Web Push and native push

The relay sends Web Push notifications to browsers, and APNs and FCM
notifications to the Aogiri app, for connected hosts; it keeps no
subscriptions and stores nothing it passes on.

- `GET /relay/v1/push/key` → `{"key":"<VAPID public key, base64url>"}`: the
  `applicationServerKey` a browser subscribes with.
- `POST /relay/v1/push`, body
  `{"pub":"<host Ed25519 public key>","ts":<ms>,"nonce":"<random>","subscription":{"endpoint"},"encrypted":"<message>","urgency","ttl","topic"}`,
  header `X-LynShen-Signature`: base64url Ed25519 signature over
  `"lynshen-relay-push-v1:" + body`.
  - `encrypted`: the notification already encrypted for the browser by the
    daemon (RFC 8291 `aes128gcm`, one record, at most 4096 bytes), so the
    relay passes on ciphertext it cannot read; the relay adds its VAPID
    `Authorization` (RFC 8292, ES256, `aud` the push service's origin, valid
    12 h) and posts it to `endpoint`.
  - A daemon before 0.7.6 sends `"payload":{…}` (plaintext, at most 3 KB) and
    `subscription.keys` instead; the relay encrypts it and forgets it.
  - `urgency` (`low`, `normal`, `high`), `ttl` (seconds, default 86400, at
    most 28 days) and `topic` (at most 32 base64url characters) become the
    RFC 8030 headers. `nonce` only makes the signature unique.
  - Accepted only from a host connected now, within 5 minutes of `ts`, a
    signature not seen before, at most 60 per host per minute (and 120 per
    IP), for endpoints of the browser push services (`fcm.googleapis.com`,
    `android.googleapis.com`, `updates.push.services.mozilla.com`,
    `*.push.apple.com`, `*.notify.windows.com`; https, no port). Replies
    `204` sent, `410` the subscription is gone (forget it), `401` bad or
    replayed signature or stale `ts`, `403` host not connected or not a push
    service, `400` bad body, `413` too large for the push service, `429`,
    `502` the push service failed.
- The same request with `"subscription":{"provider":"apns","token":"<hex>","environment":"production"|"sandbox"}`
  or `{"provider":"fcm","token":"<registration token>"}` and a plaintext
  `payload` goes to the Aogiri app (see "Native push"). `"provider":"getui"`
  with `client_id` is reserved for 个推 (Getui) and answers `501` (not
  implemented).

### Native push (APNs, FCM)

Apple and Google can read what goes through them, so a daemon sends a
short generic notice and ids, never a command, a path or a reply:
`payload` = `{"title","body","kind","level","tag"?,"session"?,"host"?,"call_id"?,"action_id"?,"url"?,"count"?}`.
`title` at most 80 characters and `body` at most 200; `kind` one of
`approval`, `error`, `done`, `update`, `test`; `level` one of `silent`,
`normal`, `urgent` (default `normal`); the ids at most 128 characters of
`[A-Za-z0-9._:-]`; `url` a `/remote…` path. `encrypted` is refused (`400`).
Ids are what the app needs to open the session (`host`, `session`) and to
answer an approval from the notification (`call_id` for `approve`,
`action_id` for `decide_action`).

- **APNs** (HTTP/2, token auth): `POST https://api.push.apple.com/3/device/<token>`
  (`environment` `sandbox`: `api.sandbox.push.apple.com`; development builds
  get sandbox tokens, TestFlight and App Store builds production ones), with
  `authorization: bearer <ES256 JWT {iss: team id, iat}, kid: key id>`
  (reused for 40 minutes), `apns-topic` (the bundle id, `com.lynshen.aogiri`),
  `apns-push-type: alert`, `apns-priority` 10 (5 for `silent`),
  `apns-expiration` now + `ttl`, `apns-collapse-id` = `topic`. Body:
  `{"aps":{"alert":{"title","body"},"thread-id":<session or tag>,"sound":"default"
  (not for silent),"interruption-level":"time-sensitive"|"active"|"passive",
  "category":"approval"},"body":{<kind, tag and the ids>}}`. `category`
  `approval` (the app's 允许/拒绝 actions) is set for one approval with a
  `call_id` or `action_id`. `body` is where expo-notifications reads the data.
- **FCM HTTP v1**: an OAuth 2.0 access token from the service account's
  RS256 JWT assertion (RFC 7523, scope `firebase.messaging`, reused until
  5 minutes before it expires), then
  `POST https://fcm.googleapis.com/v1/projects/<project>/messages:send` with a
  data-only message `{"token", "data":{"title","message","tag","channelId","categoryId"?,"body":<JSON of the ids>},
  "android":{"priority":"HIGH" ("NORMAL" for silent),"ttl":"<ttl>s","collapse_key":<topic>}}`.
  expo-notifications shows it itself (so the category's buttons work);
  `channelId` is `approvals` (urgent), `updates` (normal) or `quiet`
  (silent): the app creates these channels.
- Replies as for Web Push. `410` (forget the token): APNs `410`, or `400`
  `BadDeviceToken` / `DeviceTokenNotForTopic`; FCM `UNREGISTERED`,
  `SENDER_ID_MISMATCH` or `404`. The reply text names the provider's status
  and reason code. `501` when that provider is not configured.

Credentials stay on the relay's server in root-only files, passed in by
systemd like the VAPID key: `LoadCredential=apns:/etc/lynshen-relay/apns.p8`
(the `.p8` key from Apple, with `-apns-key-id` and `-apns-team-id` or
`LYNSHEN_RELAY_APNS_KEY_ID` / `LYNSHEN_RELAY_APNS_TEAM_ID`; `-apns-topic`
defaults to `com.lynshen.aogiri`) and `LoadCredential=fcm:/etc/lynshen-relay/fcm.json`
(the Firebase service account key). `-apns-key` and `-fcm-account` name the
files directly. The relay starts without either: that provider answers
`501`, and Web Push is unaffected. Push routes are served when at least one
of VAPID, APNs or FCM is configured; without VAPID `/relay/v1/push/key` is
`404` and a browser subscription gets `501`.

The relay serves Web Push with a VAPID key file: `-vapid-file <path>`, or
the systemd credential `vapid` (`LoadCredential=vapid:/etc/lynshen-relay/vapid.json`,
so the file can stay root-only). `lynshen-relay -gen-vapid <path>` writes a
new one (0600, never replacing a file) and prints its public key. Without a
key (and no APNs or FCM) the push endpoints are not served (404). Browsers' subscriptions are bound
to the key: a new key means every phone must turn notifications on again.

## 4. Noise session (client ⇄ daemon, inside one stream)

`Noise_IK_25519_ChaChaPoly_SHA256`, prologue = ASCII `lynshen-relay-v1`.
The client is the initiator and knows the daemon static key from pairing.

1. **msg 1** (client → daemon, `-> e, es, s, ss`), payload = JSON
   `{"name":"<device name>","pair":"<code>"}`. `pair` is present only on the
   first connection after scanning a link.
2. The daemon authorizes the client static key `rs`:
   - `rs` belongs to a paired, non-revoked device → allowed as that device;
   - else, `pair` is a valid unexpired code → the device is paired now,
     pending (`devices.jsonl` entry with `token_hash = hex(SHA-256(rs))`,
     `pending: true` and a 6-digit `check` from the handshake hash after
     msg 1), and msg 2 is `{"ok":false,"pending":true,"device","check"}`
     followed by `close`; it is allowed once the computer confirms it
     (`device_confirm`, daemon-protocol.md "Remote devices");
   - a pending device's key → msg 2 `pending` again, with its first `check`;
   - else → the daemon sends msg 2 with payload `{"ok":false,"error":"..."}`
     and then `close`.
3. **msg 2** (daemon → client, `<- e, ee, se`), payload
   `{"ok":true,"device":"<device id>","name":"<device name>"}`.
4. **Transport**: after msg 2 both sides split into cipher states. Each WS
   message is one Noise transport message. Noise messages are ≤ 65535 bytes,
   so daemon protocol frames (one JSON document each, as on the local
   WebSocket) are chunked:

   ```
   plaintext = flag (1 byte: 0 = last chunk, 1 = more follows) || chunk
   ```

   chunks of up to 65000 bytes; the receiver concatenates until flag 0 and
   parses the result as one daemon protocol frame (UTF-8 JSON).

After the handshake the stream behaves exactly like a device-authenticated
local WebSocket: `hello` first, the same ops and events
(`docs/daemon-protocol.md`, `docs/serve-protocol.md`), the same restrictions
on device clients. Revoking a device closes its relay streams.

## 5. Daemon configuration

`lynshen daemon` flags / config:

- `--relay <wss url>` (default `wss://app.lynshen.org/relay/v1`),
  `--no-relay` to disable. Desktop turns it on with a setting
  ("允许通过中继远程访问").
- New local-client ops (not allowed for device clients):
  - `relay_status` → `{type:"relay_status", enabled, connected, host, url}`
  - `pair_link` → like `pair_start` but also returns
    `{link:"https://app.lynshen.org/remote#pair=..."}`; an error while the
    relay is off.
  - `relay_set {enabled}` → turns the relay on or off (persisted in
    `settings.json`) and replies `relay_status`. Off by default.

## 6. Test vectors / interop

`crates/daemon` ships `examples/noise_peer.rs`: a responder that reads Noise
messages as hex lines on stdin and writes replies as hex lines on stdout, with
a fixed static key given on the command line. The JS side has a Node test that
runs its initiator against it (handshake, one chunked frame each way). Keep
both implementations in step with this document.
