# Relay protocol (v1 and v2)

Lets a paired phone (the PWA at `https://app.lynshen.org`) reach a desktop's
`lynshen daemon` from outside the LAN. §1–§6 describe v1 (pairing codes, no
account). §7 describes v2, where remote access belongs to the LynShen account.
The relay serves both at once: v1 stays until the Aogiri releases that speak
v2 have replaced the old ones (App Store review delays phone updates). Three
parties:

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

### Live Activity push (APNs)

The Aogiri app shows one Live Activity per computer in the Dynamic Island
(`ComputerActivityAttributes`). A daemon starts, updates and ends it with
the same signed `POST /relay/v1/push`, with an APNs Live Activity token in
`subscription` and a `liveactivity` object instead of `payload`:

```
{"pub","ts","nonce","ttl"?,
 "subscription":{"provider":"apns","token":"<hex>","environment":"production"|"sandbox"},
 "liveactivity":{"event":"start"|"update"|"end","content_state":{…},
   "stale_date"?,"dismissal_date"?,"relevance_score"?,"timestamp"?,
   "attributes_type"?,"attributes"?,"alert"?:{"title","body"}}}
```

- `token`: a push-to-start token (`start`) or an activity's push token
  (`update`, `end`), hex, 32-512 characters (current iOS gives 128 bytes).
- `content_state`: exactly the ContentState fields, all present:
  `phase` (`waiting`, `running`, `done`, `failed`), `title`, `detail`,
  `session` (an id, may be empty), `sessionTitle`, `ember` (`calm`,
  `thinking`, `done`, `attention`) strings of at most 200 characters;
  `tasks`, `waiting`, `done`, `total` counts (integers 0-100000); `since`
  (seconds since 1970, a number). Another field, or a field of another type,
  is refused. As for native notices, the daemon puts no command, path or
  reply in it.
- `stale_date`, `dismissal_date` (`end` only), `timestamp`: Unix seconds.
  `timestamp` defaults to now and must be within the last hour (and not
  ahead by more than 5 minutes); the dates within 12 hours before and
  12 hours (`dismissal_date`: 4 hours) after now.
- `relevance_score`: 0-100.
- `attributes_type` and `attributes` for `start` only: `attributes_type`
  must be `ComputerActivityAttributes`, `attributes` exactly
  `{"host":<id>,"hostName":<at most 64 characters>}`.
- `alert` (`title` at most 80 characters, `body` 200) only for `start` or
  `update` with `phase` `waiting`.

The relay sends `POST /3/device/<token>` with `apns-push-type: liveactivity`,
`apns-topic` the app topic plus `.push-type.liveactivity`
(`com.lynshen.aogiri.push-type.liveactivity`), `apns-priority` 10 for
`start`, `end` and an update with `phase` `waiting`, `done` or `failed` (or
an alert), 5 for a plain step update, and `apns-expiration` now + `ttl`
(default 600 s, at most 3600). Body:
`{"aps":{"timestamp","event","content-state","stale-date"?,"dismissal-date"?,"relevance-score"?,"attributes-type"?,"attributes"?,"alert"?}}`,
at most 4096 bytes (else `413`). Replies as for native push (`410`: forget
the token). `liveactivity` with any other provider or with a browser
subscription is `400`; without APNs configured, `501`. The relay keeps
nothing.

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

## 7. Version 2: remote access through the account

Upstream reference: the account-based relay of the upstream CLI (its
`docs/relay-protocol.md` v2). Ours differs in four ways:
- v1 keeps working next to v2;
- the relay checks tokens offline;
- push stays on this relay;
- LAN and Tailscale direct connections stay as an advanced option.

### 7.1 Tokens

Every v2 socket presents a **relay token** that the LynShen gateway signed
(gateway spec `account-computers.spec.md` AC-T3):

```
lrt1.<base64url(JSON claims)>.<base64url(Ed25519 signature over "lynshen-relay-token-v1:" || claims part)>
claims: {"aud":"lynshen-relay","kid","sub":<user id>,"authz":<device authorization id>,
         "client":"lynshen-desktop"|"lynshen-cli"|"aogiri-phone"|"lynshen-web","name","iat","exp"}
```

- A device gets a token with its device key: `POST /api/desktop/oauth/relay-token`.
  The token is valid for 10 minutes. Clients get a new one for each connection.
- Device keys come from the gateway's device flow (`POST /api/desktop/oauth/device`,
  then `…/token`), one client id per app. The desktop signs the computer in as
  `lynshen-desktop`. A computer without the desktop signs in with
  `lynshen login --device` (client `lynshen-cli`, key in auth.json
  `providers.lynshen-cli`); the daemon picks it up without a restart.
  `lynshen logout --device` forgets it.
- The relay verifies tokens without a call to the gateway. It reads the public
  keys from `GET /api/desktop/oauth/relay-key` (every 10 minutes; `-relay-keys`
  is the fallback until the gateway answers). A token is valid when its
  signature verifies with the key named by `kid`, `aud` is `lynshen-relay`,
  and `iat − 60 s ≤ now < exp`.
- Revocation: the relay sends a heartbeat to the gateway every 60 s (§7.5).
  The gateway names hosts and device authorizations that are no longer valid,
  and the relay closes them with 4403. A removed login therefore loses relay
  access within 60 s, and a token is never valid for more than 10 minutes.
- The relay holds no device keys. A token works only for the relay.

**On the upgrade request:** native clients and the daemon send
`Authorization: Bearer <token>`. Browsers cannot set headers on a WebSocket.
They offer the subprotocols `lynshen-relay` and `lynshen-bearer.<token>`, and
the relay answers `lynshen-relay`. A missing or invalid token fails the
upgrade with HTTP **401**.

### 7.2 Host connection: `GET /relay/v2/host?host=<host_id>`

- The token must belong to a computer client (`lynshen-desktop` or
  `lynshen-cli`). Another client gets HTTP 403.
- The challenge is the same as in §3.1. The `auth` message adds the daemon's
  Noise key, its name and its platform:
  ```
  {"t":"auth","pub","sig","v","static":"<x25519 static pub>","name":"<hostname>","platform":"macos|windows|linux"}
  ```
- The relay checks the signature (4401 when bad) and checks that
  `HostID(pub) = host` (4400 when not).
- The relay then registers the computer with the gateway (`POST
  /api/relay/v1/computers`). The gateway records the computer under the
  token's account and keeps a name the user changed. A host that moves to
  another account leaves the old account's list.
- Registration refused: 4401. Gateway error: 1011.
- Success: `{"t":"ready","host","user"}`.
- A newer connection for the same host replaces the older one (4409), as in v1.
- The frames are the same as in §3.1, with one change: the `open` payload
  names the caller (§7.3).

### 7.3 Client connection: `GET /relay/v2/connect?host=<host_id>`

Any client token is accepted (phone, web, desktop, CLI). After the upgrade:

| Close | When |
| --- | --- |
| 4403 `not your computer` | the host is registered under another account, or it is a v1 host (no account) |
| 4404 `host offline` | no host with this id is connected |
| 4429 | 32 streams on the host, or 20 new connections per IP per minute |
| 4503 `server restarting` | the relay is draining; reconnect after 1 s |
| 4403 `revoked` | later: the caller's login was removed (§7.5) |

When the connection is accepted, the host receives `open sid payload` with:

```
{"user":"<user id>","authz":"<device authorization id>","client":"<client id>","name":"<device name>"}
```

The rest is the same as §3.2. A v1 client (`/relay/v1/connect`) that reaches
a v2 host sends `open` with the payload `{"v":1}`. The daemon admits it only
as a device that was paired before.

### 7.4 Noise and authorization in the daemon

The Noise handshake is the same as §4, with the prologue `lynshen-relay-v1`.
In msg 1 the client can also send `"request": true`. The daemon authorizes
the client's static key in this order:

1. The `open` user is not the account the computer is registered under:
   refused, `wrong account`.
2. The key belongs to a paired device that is not revoked: allowed.
3. `pair` is a valid code: the device is paired, as in §4. The 6-digit check
   code is shown on both sides, and the computer confirms the device.
4. `request` is true: a **connect request**. The daemon sends
   `connect_request {id, name, client, check, at}` to the local clients and
   waits up to 120 s for `connect_decide {id, allow}`.
   - No local client connected: `no_desktop`.
   - Three requests already open: `busy`.
   - Otherwise the result is allowed, `denied` or `expired`.
   - The daemon sends `connect_request_closed {id, outcome}` when the request
     ends.
   - Allowed: the device is paired with the open's `user`, `authz` and
     `client`.
5. Otherwise: refused, `not paired`.

A device paired in step 3 or 4 keeps our device scopes. The `check` code is
the pairing check code of §4, and the desktop shows it in the request dialog.

### 7.5 Heartbeat (relay → gateway, every 60 s)

`POST /api/relay/v1/heartbeat` with the relay service token. The body lists:
- the connected v2 hosts with `{host, user, authz}`;
- the hosts that went offline since the last heartbeat;
- the distinct `{user, authz}` of the open v2 streams.

The gateway marks the listed hosts online and answers
`{drop_hosts, drop_authz}`. The relay closes each of those hosts and streams
with 4403.

### 7.6 Push

Push is unchanged: §3 "Web Push and native push", with a request signed by
the host key for a host that is connected now (v1 or v2). With v2, the daemon
reads its targets from the account (`GET /api/desktop/oauth/push/targets`)
instead of keeping phone subscriptions itself. The gateway never sees the
text of a notification: the daemon encrypts Web Push messages, and APNs and
FCM receive generic text only.

### 7.7 Relay configuration

| Flag / environment | Meaning |
| --- | --- |
| `-gateway` / `LYNSHEN_RELAY_GATEWAY` | the gateway origin. v2 is served only when this is set. |
| `-service-token-file`, systemd credential `relay-service`, or `LYNSHEN_RELAY_SERVICE_TOKEN` | the gateway's relay service token (≥ 32 bytes) |
| `-relay-keys` / `LYNSHEN_RELAY_TOKEN_KEYS` | token keys to use until `relay-key` answers |
| `-no-v1` | stop serving `/relay/v1/host` and `/relay/v1/connect`. Use it only when no phone from before v2 is in use. |

On SIGTERM the relay drains: v2 hosts and streams get 4503 (reconnect in
1 s), and v1 connections get 1001. `GET /relay/v2/healthz` → `200 ok`.
