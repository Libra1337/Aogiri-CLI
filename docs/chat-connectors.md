# Chat connectors

A chat connector lets the user drive the daemon's sessions from a chat app:
send a task, see progress, get the answer, allow or deny an approval, get
notifications, and send or receive files. The user creates their own bot on
the chat platform. The daemon holds the bot's credentials and connects out to
the platform (long polling or a WebSocket), so it needs no public address and
no LynShen server sees the messages.

Platforms:

| Platform | Transport | State |
| --- | --- | --- |
| Telegram | Bot API, `getUpdates` long polling over HTTPS | Implemented |
| Discord | Gateway WebSocket (direct messages only) and REST | Implemented |
| QQ official bot | WebSocket gateway (single chats) and OpenAPI | Implemented (no buttons, no edits, no files out yet) |
| WeChat ClawBot, WeCom, Feishu | | Later batches |

Background and the decisions behind this design: the owner's decision of
2026-10-16 (chat bots in the user's own daemon, private chats only, limited
approvals from chat). Group chats are not supported: the daemon ignores every
message that does not come from a private chat.

## Trust model

A chat binding is a restricted paired device. It is less trusted than a
paired phone, because:

- the chat platform reads every message;
- a chat account is often signed in on several devices;
- a stolen phone or a hijacked chat account gives direct access to the bot.

Rules:

1. Only senders on the allow-list reach a session. The allow-list holds the
   platform's immutable user id of each bound account, never a user name
   (user names can change).
2. A message from any other sender never reaches a model. The daemon ignores
   it. It answers only a bind attempt, and only while a binding code is
   pending.
3. Only private chats count. Messages in groups, channels and threads are
   ignored, including messages from bound accounts.
4. The text of a bound sender's own message goes to the session as the
   user's message. A forwarded message is refused, because its content comes
   from another sender.
5. The chat profile (below) limits what a binding can do. The daemon checks
   it for every op of a chat client, whatever the connector code sends.
6. Bot credentials stay on this computer. They are never logged, never shown
   to a client after they are saved, and never sent to a chat.

## Setup

1. The user creates a bot on the platform and copies its token.
   - Telegram: @BotFather `/newbot`.
   - QQ: QQ 开放平台 → 创建机器人; the AppID and the AppSecret (开发设置).
     An unverified bot talks with its admin and up to 20 test accounts,
     which is enough for one's own bot. In 开发设置 → 回调配置 choose the
     WebSocket mode.
   - Discord: Developer Portal → New Application → Bot → Reset Token. The
     bot takes direct messages only from people it shares a server with:
     the user adds it to a server of their own with the connector's `link`
     (an OAuth2 link with the `bot` scope and no permissions).
2. A local client (the desktop's 设置 → 聊天连接) sends `connector_add`. The
   daemon checks the token with the platform (Telegram: `getMe`) and saves
   the connector.
3. The client sends `connector_bind_start` and shows the returned code: an
   8-character code, valid 5 minutes, single use. For Telegram it also shows
   `link` (`https://t.me/<bot>?start=<code>`) as a link and a QR code.
4. The user sends the code to the bot in a private chat: open the link
   (Telegram sends `/start <code>`), or send `/bind <code>` or `绑定 <code>`
   (Discord: a direct message to the bot).
5. The daemon records the sender as a binding and answers in the chat.
   Every local client receives `connector_bound` and a new `connectors`
   list.

Use one bot for each computer. Telegram answers HTTP 409 when two programs
poll one bot; the connector then shows the error and does not fight for the
connection. When the daemon finds its state copied from another computer, it
turns every connector off (and unpairs every device, chat bindings too).

Brute force: after 5 wrong codes from one sender within 10 minutes, the
daemon ignores that sender's bind attempts for 1 hour.

## State files

In `~/.lynshen/daemon/`:

| File | Contents |
| --- | --- |
| `connectors.json` | The connectors: platform, bot name, settings, proxy and credentials. Mode 0600. With `"encrypt_secrets": true` in config.json and a secret key present (`docs/secrets.md`), each credential is a `jcenc1:` envelope. |
| `connectors-state.json` | Each connector's position in the platform's update stream (Telegram `offset`) and each binding's current session. Mode 0600. |
| `connectors.jsonl` | Audit log: connectors added and removed, bindings made, refused and revoked, bind lockouts, approvals and decisions from chat (session, call or action id, decision), chat command names and refused requests. It holds no message content. |

Bindings are devices in `devices.jsonl` with `device_kind: "chat"` and the
connector, platform and peer (the sender's user id). A binding's token hash
is the hash of a random value that is never stored, and the daemon never
accepts a chat binding as a WebSocket or relay device.

## Ops

All connector ops are local-only, like `relay_set`: a paired phone or a chat
binding cannot read or change connectors or credentials.

| Op | Fields | Reply |
| --- | --- | --- |
| `connector_list` | — | `connectors` |
| `connector_add` | `platform` (`telegram`, `discord` or `qq`), `token` (QQ: `app_id` and `secret`), optional `proxy`, `api` | `connector_added` with `connector`; every local client receives `connectors`. The daemon checks the token first; a wrong token or no connection is an error, and nothing is saved |
| `connector_set` | `connector`, any of `enabled`, `proxy`, `token` (QQ: `secret`), `approvals`, `notify`, `max_mode` | `connectors`. A new `token`, `proxy` or `enabled` restarts the connection |
| `connector_remove` | `connector` | `connector_removed` with `connector`; its bindings are revoked and its credentials deleted |
| `connector_bind_start` | `connector` | `connector_bind_code` with `connector`, `code`, `expires_at` and `link` (Telegram; `null` for other platforms) |

`connectors`, sent to local clients only, after every change and when a
connection's state changes:

```json
{"type":"connectors","connectors":[{
  "id":"tg-3f9a1c2b","platform":"telegram","name":"@my_lynshen_bot",
  "link":"https://t.me/my_lynshen_bot","enabled":true,
  "proxy":null,"proxy_source":"environment","proxy_shown":"http://127.0.0.1:7890",
  "state":"connected","error":null,
  "settings":{"approvals":true,"notify":true,"max_mode":"auto"},
  "bindings":[{"device":"dev-…","name":"Alice (@alice)","paired_at":0,"last_seen":0}],
  "created_at":0}]}
```

- `link`: Telegram, where the bot opens (`https://t.me/<bot>`); Discord,
  the link that adds the bot to a server.
- `state`: `stopped` (disabled), `connecting`, `connected` or `error`
  (`error` says why, without credentials).
- `proxy`: the connector's own setting: `null` (use the environment),
  `"direct"` (no proxy) or an `http://host:port` URL. `proxy_source` says
  what the connection uses now: `setting`, `environment` or `none`.
  `proxy_shown` is that proxy without user name or password.
- `settings` (per connector, for all of its bindings):
  - `approvals` (default `true`): bindings may allow or deny ordinary
    approval requests. `false`: approval cards only point to the desktop.
  - `notify` (default `true`): bindings get notifications (below).
  - `max_mode` (default `auto`): the loosest approval mode a binding may use
    or drive: `plan`, `manual`, `auto-edit` or `auto`. `full-access` is never
    allowed.

`connector_bound` (`connector`, `device`, `name`) goes to local clients when
a binding is made. `device_list` lists bindings with the other devices; each
device carries `kind` (`device` or `chat`), and a binding also `platform`,
`connector` and `last_seen` (ms, the last message from that account since
the daemon started, else `null`). `device_revoke` removes a binding: its
client stops at once and later messages from that account are ignored.

## Network and proxy

Telegram (and Discord) cannot be reached from mainland China without a
proxy. A connector uses, in this order:

1. its `proxy` setting: an HTTP proxy URL (`http://host:port`, with optional
   `user:pass@`), or `direct`;
2. the engine's proxy settings: `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY`,
   with `NO_PROXY` exempting hosts, read as web_fetch reads them. The
   desktop starts the daemon with the login shell's environment, so a proxy
   set in the shell profile applies.

SOCKS proxies are not supported. Clash and similar clients also offer an HTTP
port; use that one.

On a failed connection the connector waits 1 s, doubling to 60 s, plus up to
1 s of random delay, then connects again. Telegram resumes from the saved
`offset`, so no update is lost while the platform keeps it (24 hours).

## The chat profile

Every op of a chat client goes through the same `handle()` as a phone's ops,
plus these checks. Ops not in this table are refused.

| Op | Allowed from chat |
| --- | --- |
| `ping`, `session_list`, `session_history`, `workspaces`, `agent_list`, `question_list`, `actions_list`, `report_list` | Yes |
| `session_create` | Only a LynShen session in a known project's main directory, or `chat: true`. No `engine`, `agent` or `options` |
| `session_open`, `watch`, `unwatch` | Yes, by session id only |
| `user_message`, `steer` | Only to a session whose approval mode is known and no looser than `max_mode`. `images` must be uploads |
| `interrupt`, `question_answer` | Yes |
| `approve` | `decision` `allow` or `deny`, for a pending request of that session, while `approvals` is on. Refused: `always`, a `scope` other than `once`, `prefix`, `hunks`, `add_dir`, and any request with `kind: "elevation"` |
| `decide_action` | An open deferred action, while `approvals` is on. Allowing an action with `kind: "elevation"` is refused |
| `set_approval_mode` | Only to a mode no looser than `max_mode`; never `full-access` |
| `approve_plan` | `approve` with a `mode` no looser than `max_mode`, or `revise` |
| `fs_read`, `git_status`, `git_diff` | Yes, with the rules of `fs_*` (known directories, credentials unreadable) |
| `term_*`, `session_tui` | No: "open a terminal on the desktop or the phone page" |
| Everything else (`project_*`, `workspaces_set`, `agent_*` writes, schedules, requirements, MCP, skills, devices, relay, connectors, uploads, `command`, `set_sandbox`, `dispatch_*`, `message_send`, `push_*`) | No |

Thus, from a chat, nobody can:

- switch a session to full access (全自动 or 完全访问), or drive a session
  that runs in it;
- allow a call "always" (for the session or the project);
- approve sudo or another elevation;
- open a terminal or a session's TUI;
- add directories that `fs_*` may read.

The bot says so in plain words and points to the desktop or the phone page
(`https://app.lynshen.org/remote`).

## In the chat

| Message | Does |
| --- | --- |
| `/start <code>`, `/bind <code>`, `绑定 <code>` | Binds this account (above) |
| `/help`, `帮助` | The command list |
| `/new [n\|name]`, `新建` | A new session in a project. Without an argument it lists the projects with numbers |
| `/chat`, `聊天` | A new chat session (`chat: true`: no project files) |
| `/sessions`, `会话` | The 10 most recent sessions, numbered, with their state and a mark when an approval waits |
| `/use <n>`, `切换 <n>` | Makes a session from `/sessions` the current one |
| plain text | Goes to the current session as the user's message. While a turn runs, the engine queues it. With no current session, a new chat session starts |
| `/stop`, `停止` | `interrupt` |
| `/status`, `状态` | The current session's title, project, state, approval mode and waiting approvals |
| `/mode [mode]`, `模式` | Shows or sets the approval mode: `plan` (计划), `manual` (谨慎), `auto-edit`, `auto` (自动), up to `max_mode` |
| `/diff` | The current project's `git status` summary, and the diff as a `.diff` file |
| `/file <path>` | Sends a file from the current session's directory (relative or absolute path) |
| `/open` | The phone page's address, for full details end to end encrypted |
| `允许 <id>`, `拒绝 <id>` (`/allow`, `/deny`) | Answers an approval card when the platform has no buttons |
| `/unbind`, `解绑` | Removes this binding (the same as revoking it on the desktop) |

A binding has one current session, saved in `connectors-state.json`. A
session started from chat is a LynShen session; when its approval mode is
looser than `max_mode`, the connector sets it to `max_mode` before the
first message.

The bot answers in Chinese, or in English when the platform says the
account's language is not Chinese (Telegram's `language_code`).

### Progress and replies

When a turn that the chat started begins, the bot posts one status message
(思考中). It edits that message at most every 3 seconds with the current
step: the tool and its command (`运行 cargo test`) or the current plan step.
When the turn ends, the bot sends the final reply as new messages, so the
phone notifies, and edits the status message to `完成 · 用时 2 分 13 秒`
(`已停止` after an interrupt, `出错` with the error).

- The final reply is the text of the turn's last response. Tool outputs and
  file contents are not posted.
- Telegram gets HTML (code blocks, inline code, bold). If Telegram refuses
  the HTML, the bot sends the text plain.
- A reply longer than the platform's limit (Telegram 4096 characters) is
  split at paragraph and code-block boundaries. After 3 parts, the rest goes
  as a `.md` file.
- A turn that the desktop or the phone started in the same session is not
  posted.

### Approvals

The binding's client watches its current session while the chat is active:
the last message from the account came less than 30 minutes ago, or a turn
the chat started still runs. While it watches, the session is attended and
an approval request waits for an answer, as on the desktop. After that it
unwatches, and approvals become deferred actions again.

An approval card shows the session, the kind in words (`越界命令`,
`写工作区外文件`, `规则要求确认`, ...), the command or summary (500
characters) and the model's reason, all redacted (below). It has two
buttons, 允许 and 拒绝, and a 5-letter id for the text fallback. Only the
bound account's presses count; an id is single use and expires after 30
minutes. An elevation card has no buttons: it says to handle it on the
desktop or the phone page.

The desktop, the phone page and the chat all get the request; the first
answer wins. When another client answers, the bot edits the card to
`已在其他设备处理`.

Deferred actions (from sessions that nobody watched) come as notifications
with the same buttons, which send `decide_action`.

A plan (`proposed_plan`) of the current session comes with 批准 and 修改.
批准 sends `approve_plan` with the mode `auto-edit` (or `max_mode` when that
is stricter). For 修改, the user replies to the plan message with what to
change. A question (`question` tool) of the current session comes as a
message; a reply to it answers the question.

### Notifications

While `notify` is on, each binding gets:

- deferred actions, with buttons;
- what the phone gets through Web Push: dispatch plans and approvals,
  finished dispatches, requirements that wait for the user, agents' failed
  turns and closed items;
- questions of the current session.

The same notification (by its tag) is sent at most once a minute.

### Files

- In: a photo or a file sent to the bot is downloaded (Telegram: at most
  20 MB) into the uploads directory (`daemon-protocol.md`, "Uploads"). With
  a caption it goes to the session at once; without one, it waits for the
  next text message. Images go in `images`; other files are listed after the
  text under `Attached files (read these):`, as the desktop does.
- Out: `/file` sends a file of at most 50 MB (Telegram) from the current
  session's directory, with the rules of `fs_read`. It also refuses names
  that usually hold secrets: `.env*`, `*.pem`, `*.key`, `id_*`, `*.p12`,
  `*.pfx`, `.npmrc`, `.pypirc`, `.netrc`, `credentials*`.

### Redaction

All text the bot sends passes a redaction filter. These become
`[已隐藏疑似密钥]`:

- PEM private keys;
- tokens that start with `sk-`, `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`,
  `github_pat_`, `xoxb-`, `xoxp-`, `xoxa-`, `xoxr-`, `AKIA`, `ASIA`,
  `AIza`;
- JSON Web Tokens;
- Telegram and Discord bot tokens, and the credentials of every connector;
- the value in `NAME=value`, `NAME: value` and `"name": "value"` when the
  name contains `key`, `token`, `secret`, `password`, `passwd`,
  `credential` or `auth`.

A path to a credentials file or directory (`.ssh/`, `.aws/`, `.gnupg/`,
`.lynshen/auth.json`, `.lynshen/secret.key`, `.lynshen/daemon/`,
`.netrc`, `.git-credentials`, `.docker/config.json`) becomes
`[已隐藏凭据路径]`.

## Telegram

- API: `https://api.telegram.org/bot<token>/<method>`. `api` in
  `connector_add` replaces the origin (a local Bot API server, or a test
  server).
- Receiving: `getUpdates` with `timeout: 25` and `allowed_updates`
  `message` and `callback_query`. Only `chat.type: "private"` counts. The
  sender is `from.id`.
- Sending: `sendMessage` (`parse_mode: "HTML"`, inline keyboard for
  buttons), `editMessageText`, `answerCallbackQuery`, `sendDocument`
  (multipart). Callback data is `a:<id>:y` or `a:<id>:n` and stays under
  Telegram's 64 bytes.
- The bot's command menu is set with `setMyCommands` when the connector
  starts.
- HTTP 429: the bot waits `retry_after` (at most 30 s) and tries once more.
- HTTP 401: the token is wrong or was revoked; the connector stops with an
  error until the token changes.
- HTTP 409: another program polls this bot, or the bot has a webhook; the
  connector shows the error and tries again after 60 s.

## Discord

- REST: `https://discord.com/api/v10` with `Authorization: Bot <token>`;
  `api` replaces it (a test server). `GET /users/@me` checks the token.
- Receiving: the Gateway from `GET /gateway/bot`, `?v=10&encoding=json`,
  through the connector's proxy (an HTTP CONNECT tunnel). Identify with
  `intents: 4096` (direct messages); a bot gets the text of direct messages
  without the privileged message content intent. Heartbeats at the
  interval Hello names, the first after a random part of it; a connection
  with an unanswered heartbeat is dropped. After a drop the connector
  resumes the session (`resume_gateway_url`, op 6) when it can, else it
  identifies again.
- Only messages without `guild_id` count (direct messages). A forward
  (`message_reference.type` 1, `message_snapshots`) is refused.
- Sending: `POST /channels/<id>/messages` with Markdown as it is (2000
  characters; the bot splits at 1900), `allowed_mentions` empty so a reply
  never pings anyone; buttons as one action row (允许 primary, 拒绝 danger);
  `PATCH` edits; files as multipart `payload_json` and `files[0]`, at most
  20 MiB both ways.
- A button press (`INTERACTION_CREATE`) is answered at once with type 6
  (the message changes when the bot edits it), as Discord wants within 3
  seconds; the press then goes to the chat side, which checks the presser.
- HTTP 429: the bot waits `retry_after` (at most 30 s) and tries once more.
- Close codes 4004 (token) and 4013, 4014 (intents) stop the connector
  until its settings change; 4007 and 4009 start a new session.
- The bot answers in Chinese: Discord does not report the account's
  language in a message.

## QQ

- Credentials: the AppID and the AppSecret. The connector signs in with
  `POST https://api.bot.qq.com/app/getAppAccessToken` for an access token
  (about 2 hours) and fetches a new one a minute before it runs out. The
  AppID is saved as it is; the AppSecret like any token (`jcenc1` when
  asked). `api` replaces the origin (a test server). QQ needs no proxy in
  mainland China.
- OpenAPI: `https://api.bot.qq.com` with `Authorization: QQBot <access
  token>` and `X-Union-Appid`.
- Receiving: the gateway from `GET /gateway`, Identify with
  `intents: 33554432` (`GROUP_AND_C2C_EVENT`, 1 << 25) and `shard: [0, 1]`,
  heartbeats at the interval Hello names, Resume (op 6) after a drop. Only
  `C2C_MESSAGE_CREATE` (single chats) counts; group messages are ignored.
  The sender is `author.user_openid`, fixed for the pair of user and bot.
  QQ may push one message twice: the connector drops a message id it saw.
- Close codes 4914 (offline: sandbox only), 4915 (banned), 4013 and 4014
  (events) stop the connector until its settings change; 4009 resumes;
  others identify again.
- Sending: `POST /v2/users/<openid>/messages` as Markdown (`msg_type` 2), as
  text (`msg_type` 0) when QQ refuses it. Within 55 minutes of the user's
  message, a reply answers it (`msg_id`, `msg_seq`, at most 4 times); after
  that it is an active message (QQ allows 20 a minute and 1000 a day per
  user, and the user can turn them off).
- QQ has no message edits: a turn shows no status message, only
  `仍在运行（已 N 分钟）` every 10 minutes, then the reply.
- No buttons (custom buttons need an application to QQ): approval cards
  name their id, and the user answers `允许 <id>` or `拒绝 <id>`.
- Files in: attachments by their `url`, at most 20 MB. Files out are not
  supported yet (QQ takes a local file only through its chunked upload):
  `/file` says so, `/diff` sends the summary only, and a reply longer
  than 3 messages ends with a note to read the rest on the desktop.
- The bot answers in Chinese.
