# Android phone control (android-use)

LynShen can read and operate an Android phone or emulator connected to this computer, through
[android-use](https://github.com/austinintelligence/android-use) (MIT): its `au` binary talks to the
phone over ADB and to a helper app on the phone that uses Android's Accessibility service.

It is off by default. Nothing is downloaded until the user installs it.

## Turning it on

| Step | TUI | Desktop (设置 → MCP 与扩展 → 安卓手机) | daemon op |
| --- | --- | --- | --- |
| Install android-use 1.2.0 | `/android install` | 安装 | `android_use_install` |
| Enroll the phone, install the helper | `/android setup` | 连接手机 | `android_use_setup` |
| Offer the tools to the model | `/android on` | the switch | `android_use_enable` |
| Status | `/android` | the row | `android_use_status` (`doctor: true` runs `au doctor`) |

`/android on` writes `"android_use": true` to config.json; every turn reads it again. The daemon ops are
for the desktop only.

The install downloads `android-use-v1.2.0-<os>-<arch>` from the GitHub release, checks it against the
SHA-256 pinned in `crates/agent-core/src/android.rs` (the release's `SHA256SUMS`) and unpacks it into
`~/.lynshen/tools/android-use/1.2.0`. A wrong checksum installs nothing. au keeps its state (the enrolled
phone, the action journal) in `~/.lynshen/tools/android-use/state` (`AU_HOME`).

adb is not part of android-use. LynShen looks for it in `AU_ADB`, `ANDROID_SDK_ROOT`/`ANDROID_HOME`, the
Android SDK's default folders, Homebrew and `PATH`.

## The phone

1. Settings → About phone: tap Build number seven times. Settings → Developer options: turn on USB
   debugging.
2. Connect a data cable and tap Allow on "Allow USB debugging?" (the computer's RSA key).
3. `/android setup` installs the Android-Use helper and opens Accessibility; turn Android-Use on there.

Wireless debugging (Android 11+): Developer options → Wireless debugging → Pair device with pairing
code, then `adb pair IP:PORT` and `adb connect IP:PORT`. Any paired computer can then control the phone:
turn it off afterwards.

When a call fails because of the connection, its result has `hint_for_user`, a Chinese next step (USB
debugging, the RSA prompt, accessibility, unlock the phone).

## Tools

`android_read` and `android_act` take one `command` string in au's grammar (`android_read help` lists
it). One `au serve --mcp` per process serves every session; it stops after 5 idle minutes. au allows one
server per state folder, so a second LynShen process on the same computer gets an error until the
first one stops.

A screen image au attaches goes to the model as an image (as `read` does with an image file). What the
phone shows goes only into the tool result: tool errors from the phone tools are not logged and au's
stderr is discarded.

## Approval

| Mode | `android_read` | `android_act` |
| --- | --- | --- |
| plan | runs | refused |
| manual | asks | asks |
| auto-edit | runs | asks |
| auto | runs | asks (no safety classifier) |
| full-access | runs | runs |

`android_act` can be approved once only: there is no "allow for this session". Camera, microphone and
recording commands are `must_ask`: a person decides in every mode. The approval's `summary` says in
Chinese what happens and in which app, for example 「在 微信 里点「发送」」; the app is the one an earlier
step opens, else the phone's foreground app (from `adb shell dumpsys activity activities`).
