# Sandbox

LynShen runs the model's shell commands in an OS sandbox: Seatbelt
(`sandbox-exec`) on macOS, `bwrap` (bubblewrap) on Linux. It applies to every
engine: the TUI, `lynshen serve`, `--headless`, Desktop sessions and daemon
sessions. A daemon agent uses its own settings from `agent.json`
(`docs/daemon-protocol.md`).

## Modes

| Mode | Commands can write |
| --- | --- |
| `read-only` | nothing |
| `workspace-write` (default) | the working directory, the configured read-write directories, temp and package-cache directories (`~/.cache`, `~/.npm`, `~/.cargo/registry`, …). Inside them `.git` (and a worktree's real git directory), `.lynshen` and `.agents` stay read-only, as do the configured read-only directories. |
| `full-access` | anything: no sandbox |

In every sandboxed mode `~/.ssh`, `~/.gnupg`, `~/.aws`,
`~/.lynshen/auth.json`, `~/.lynshen/daemon` and the key that decrypts
Desktop's stored secrets (`secret.key`) are unreadable. Network access
is on unless `sandbox_network` is false.

Windows has no OS sandbox by default. It defaults to `workspace-write`
too, which is not enforced there: commands run with your permissions, the
model is told the working directory is its boundary and declares crossing
it (`escalate`), the file tools keep to it, and clients see
`enforced: false` in the `sandbox` event. A daemon agent still refuses to
start with a sandbox Windows cannot enforce (its default stays
`full-access`).

### Windows sandbox (prototype, opt-in)

`"sandbox_windows": "restricted-token"` in config.json turns on a Windows
sandbox LynShen sets up itself, without administrator rights (Windows 10
1607 and later). Desktop's switch is Settings → 权限与沙箱 → "Windows 沙箱（预览）".
The default is `"off"`; the engine never writes the key. It is a machine
setting: a session reads it when it starts and again on `set_sandbox`
(Desktop sends that to the running sessions after changing it), and every
session of the process follows the value read last. With it on, `enforced`
is true and a daemon agent may run sandboxed.

- Each command starts through `lynshen __sandbox-exec`, which runs it with a
  restricted, low-integrity token in a job object. When the command ends,
  times out or is stopped, everything it started ends with the job.
- Writable: the working directory and the read-write directories, by an
  ACL entry for a capability SID made for this engine, plus an integrity
  label of Low. Inside them `.git`, `.lynshen` and `.agents` are denied to
  that SID. The command gets a private temp directory (`TEMP`, `TMP`).
  The user's temp directory and the package caches are not writable:
  granting a directory changes every file in it. Installs (`npm install`,
  `cargo fetch`, `pip install`) therefore leave the sandbox through the
  usual approval (`escalate`); the prompt tells the model so.
- Shell: Git Bash cannot run under this token (its MSYS2 runtime opens
  pipes only the user's own token may write). With the sandbox on, the
  `gitbash` setting, and `auto` where it picks Git Bash, run commands in
  PowerShell 7, else Windows PowerShell; the `shells` reply says so
  (`commands.instead_of: "gitbash"` and a note). With no PowerShell at all,
  a command is refused with that reason.
- Unreadable: the credential paths above get a Medium "no read up" label,
  which a low-integrity process cannot read through. Your own programs
  still can.
- Not restricted: the network. `sandbox_network: false` is not enforced;
  the prompt and `/sandbox` say so.
- A drive root, the profile folder or a folder around it cannot be a
  writable root: open a project folder.
- Every ACL entry and label is taken off when the engine exits. Each change
  is logged first under `~/.lynshen/sandbox`, so changes an engine left
  when it crashed are undone by the next engine that runs a command.

The crate is `crates/lynshen-sandbox` (it also holds a Landlock backend for
Linux, not used by the engine yet). Parts are adapted from OpenAI Codex's
`windows-sandbox-rs` (Apache-2.0, see NOTICE).

On Linux without `bwrap`, or where AppArmor blocks it (Ubuntu 24.04 and
later restrict unprivileged user namespaces), LynShen reports the sandbox as
unavailable at startup (`unavailable` in the `sandbox` event, with the fix)
and shell commands fail until you install bubblewrap, allow the namespaces
(`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, or an
AppArmor profile for bwrap) or switch to `full-access`; it never falls back
to running commands unsandboxed.

## Approvals

A command inside the sandbox needs no approval, except in `manual` mode,
which asks for every command that changes anything. A command `manual` mode
still runs without asking is one that provably only reads: every part of it
(a pipeline, a `&&`/`||`/`;` list, a `for … do … done` loop) is a read-only
program — `ls`, `cat`, `head`, `tail`, `grep`/`rg`, `find` (no `-exec` or
`-delete`), `wc`, `ps`, `git status`/`diff`/`log`/`show`/`branch --list`
and the like — with no redirection except to `/dev/null`, no command
substitution and nothing that writes, installs or reaches the network. This
is the same idea as Claude Code's default mode and Codex's known-safe list,
and it still runs inside the OS sandbox; anything the classifier does not
recognise falls back to asking. Only a POSIX shell (zsh, bash, sh, Git Bash)
is classified; under PowerShell every command still asks. Your `ask` rules
and the built-in ask list come first, so they keep asking even for a
read-only command.

A command that must leave the sandbox
(commit to git, write elsewhere) is called with `escalate: true` and a
`justification`, and goes through the approval mode: `auto` asks the safety
model, `manual` and `auto-edit` ask you, `full-access` runs it.

The approval mode and the sandbox are separate. `full-access` approval
keeps the sandbox (LynShen Desktop's 全自动): commands still run in it, and
what leaves it runs without asking but is reported (`escalated`, a line in
the conversation). Only the built-in list and your `ask` rules ask. Turning
the sandbox itself off is `set_sandbox` / `/sandbox full-access` (Desktop's
完全访问, in settings only). Where no OS sandbox enforces the mode
(Windows), a command without `escalate` is gated by the approval mode as
if there were no sandbox.

Command rules are checked first, by how the command starts:

- `forbid`: never runs;
- `ask`: always asks you, in every approval mode (`full-access` too). "Always
  allow" for the session does not cover it, and a switch to a looser mode
  does not approve a call that waits;
- `allow`: runs without asking, and may leave the sandbox without asking.
  Not when its text writes outside the workspace and the writable
  directories (`npm install > ~/.zshrc`): that goes to the approval mode.

`forbid` wins over other matches, otherwise the longest prefix applies; at
the same length `ask` wins over `allow`. The defaults let `git add` and
`git commit` leave the sandbox and always ask before `git push`.

A command line is matched part by part. It is split on `&&`, `||`, `;`,
`|`, `&`, new lines and command substitutions (`$(…)`, backticks); code
given inline to a shell (`bash -c "…"`, `powershell -Command …`,
`cmd /c …`, `eval`, `Invoke-Expression`, `watch`) is split the same way, and so are groups and PowerShell script
blocks (`{ … }`) and the command `find -exec` runs. From each part, leading
variable assignments and wrappers come off (`sudo`, `doas`, `env`,
`command`, `time`, `nohup`, `nice`, `timeout`, `xargs`).

- `forbid` and `ask` rules match the part in any case, as written, without
  its wrappers, or with the program named by its file name
  (`/usr/bin/git push`, `GIT.EXE PUSH` match `git push`).
- `allow` rules match only the words as written, with wrappers that do not
  change the program (`time`, `nohup`, `nice`, `timeout`, `command`) taken
  off. A variable assignment, `env`, `xargs` or elevation in front, or
  another path for the program, can make it run something else:
  `PATH=. git commit`, `env GIT_DIR=… git commit` and `./git commit` do not
  match `git commit`.

Then:

- a part that is forbidden makes the whole command forbidden;
- a part that must ask makes the whole command ask;
- the command is allowed only when every part is, except parts that only
  move around, print or filter (`cd`, `echo`, `grep`, `head`, `tail`, ...,
  without a redirection, `sort -o` or `rg --pre`). `export` and `set` are
  not among them: they change what later parts run.

### Built-in lists

Two lists live in the engine, not in the settings, and you cannot remove
them. They apply in every approval mode and every sandbox mode, Windows
included. A rule of yours does not override them; you can add your own
`ask` and `forbid` rules.

Always asks (`must_ask`, approved once at most):

- elevation: `sudo`, `su`, `doas`, `pkexec`, `gsudo`, `runas`,
  `Start-Process -Verb RunAs`;
- `git push`, forced or not;
- deleting files outside the workspace, the read-write directories, temp
  and the package caches (`rm`, `rmdir`, `unlink`, `del`, `rd`,
  `Remove-Item`, `find -delete`, `find -exec rm`, ...); out of the sandbox
  (escalated, Windows, `full-access`) also a delete whose paths the text
  does not name (`rm -rf "$DIR"`, `xargs rm`);
- changing system settings: services and startup items (`launchctl`,
  `systemctl enable|start|stop|…`, `service … start`, `sc create|config|…`,
  `New-Service`, `schtasks /create`, `crontab`), the registry under HKLM,
  the firewall and antivirus (`netsh advfirewall`, `ufw`, `iptables`,
  `pfctl`, `Set-MpPreference`, ...), `spctl`, `csrutil`,
  `Set-ExecutionPolicy`;
- reading or writing credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.lynshen/auth.json`, `~/.lynshen/daemon`);
- changing LynShen's settings (`~/.lynshen/config.json`), which hold these
  rules.

They also apply to what `write_stdin` types into a running shell.

Never runs:

- deleting the root, the home directory or a directory around it, a drive
  root, or a directory that contains the workspace (`rm -rf ~`,
  `rm -rf ~me`, `rm -rf /*`, `rmdir ..`, `find ~ -delete`).

`--headless` denies every approval request, so it refuses the commands on
the first list too, in `full-access` as well.

These lists, like the rules, match the command's text. They catch a
model's mistake, not a command written to get past them: an alias, a
script, a variable (`rm -rf "$DIR"`), an encoded or generated command
line are not seen through. What is enforced is the OS sandbox on macOS and
Linux (what a command can write) and the file tools' own checks.

### What the model is told

The system prompt says, in every mode (`full-access` and Windows too), what
commands may write, how to leave the sandbox (`escalate: true` with a
`justification`), what file tools may read and write, and which commands
always ask. A change (`/sandbox`, `set_sandbox`, a directory
added) reaches the model as a note on its next turn. When a sandboxed
command fails with "Operation not permitted", `EPERM` or "Read-only file
system", its result carries a `sandbox_hint` telling the model to run it
again with `escalate` if it must write outside.

### How far an approval reaches

A client answers a request once, for the session, or for the project
(`approve` with `scope`, `docs/serve-protocol.md`):

- **session**: for a command, commands that start with the same words
  run without asking for the rest of the session: the program and the
  words up to its first word that is not an option (`npm install`,
  `npx -y create-vite`, `make`). For a command that left the sandbox, that
  is an `allow` rule: inside or outside the sandbox. For a command that ran
  in the OS sandbox (`manual` mode asks for those), it stays inside: the
  same command still asks to leave it. For a file edit or another tool,
  that tool stops asking. The shell is never allowed as a whole.
- **project**: the prefix is saved as an `allow` rule for the project in
  `~/.lynshen/config.json` (`projects`, see "Settings"), or for every
  project with `always_allow_scope: "global"`. Nothing is written into the
  repository. Not offered for a command that ran in the OS sandbox. A rule
  that asks or forbids the same prefix is never turned into an allow.

A client may lengthen the prefix with the command's own next words, never
shorten it or add other words.

Only once, never for the session or the project: requests that must be
asked, interpreters (`python`, `node`, shells: `bash -c`, `bash build.sh`,
`powershell -Command`, `source`, ...), deletes (`rm`, `Remove-Item`, ...),
elevation (`sudo`, `runas`, ...), `git push --force`, a command whose text
writes outside the workspace (make the directory writable instead), a part
with a variable assignment, `env` or `xargs` in front (an allow rule would
not match it), and a command whose parts start differently
(`make && npm test`).

An action deferred while no client watched and approved later is decided
for that call only when it must be asked: the same call again is deferred
again.

## File tools

The file tools run in-process, so the OS sandbox does not apply to them;
they check paths themselves.

- **Reading** (`read`, `ls`, `ripgrep`, `outline`, image inputs): anywhere,
  except credentials: `~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.lynshen/auth.json`, `~/.lynshen/daemon` (and `%APPDATA%\gnupg` on
  Windows), also from a workspace that contains them (a workspace at
  home). Paths are compared with symlinks and `..` resolved and in the
  file system's own case.
- **Writing** (`write`, `edit`, `hashline_edit`, `generate_image`): inside
  the workspace, the read-write directories, temp and the package caches,
  with `.git`, `.lynshen` and `.agents` read-only and nothing writable in
  `read-only`. A write anywhere else is an `outside_write` approval: asked
  in `manual`, `auto-edit` and `plan`, put to the safety model first in
  `auto`, and run without asking in `full-access`, which reports it
  (`escalated`). A write into credentials or `~/.lynshen/config.json`
  always asks, wherever it is (inside the workspace or a writable
  directory too). It is approved once at a time, for that path; to stop
  asking, make the directory writable (`approve` with `add_dir`, or
  `add_directory`). The root, the home directory and the directories
  around it, credentials and LynShen's settings cannot be made writable.
  `generate_image` names its files itself, so it asks for the directory
  they go into, and creates new files only, right in it.
- `apply_patch` changes files inside the workspace only.
- Checkpoints and `/rewind` cover files inside the workspace only: a file
  written outside it is not restored, and the tool result says so.
- Deleting outside the workspace is done with a command (`rm`, ...), which
  the built-in list always asks about.

## Worktree subagents

A subagent that works in its own git worktree (`isolation: "worktree"`, the
`worker` role; see `docs/serve-protocol.md` → "Agent team") is confined to it:

- Its commands start in the worktree; a `workdir` outside it is refused.
- In the sandbox, the worktree, temp and package-cache directories are its
  only writable places. The project around the worktree and the configured
  read-write directories are read-only for it, even where they lie in temp.
  The worktree's `.git` file and its real git directory stay read-only.
- Leaving the sandbox (`escalate: true`) always asks you, whatever the
  approval mode or "always allow", unless an `allow` command rule matches
  (`git commit` by default).
- Its file tools write only inside the worktree; a write elsewhere is
  refused, not asked.

With the sandbox off (`full-access`, Windows) only the first and last points
hold: a command started in the worktree can still write elsewhere by an
absolute path.

## Settings

In `~/.lynshen/config.json`:

```json
{
  "sandbox": "workspace-write",
  "sandbox_network": true,
  "sandbox_directories": [
    { "path": "/srv/deploy", "mode": "rw" },
    { "path": "/var/log/app", "mode": "ro" }
  ],
  "command_rules": [
    { "prefix": "git add", "action": "allow" },
    { "prefix": "git commit", "action": "allow" },
    { "prefix": "git push", "action": "ask" }
  ],
  "always_allow_scope": "project",
  "projects": {
    "/Users/me/code/app": {
      "sandbox_directories": [{ "path": "/Users/me/code/shared", "mode": "rw" }],
      "command_rules": [{ "prefix": "npm install", "action": "allow" }]
    }
  }
}
```

- `projects`: settings for one workspace, keyed by its absolute path with
  symlinks resolved (LynShen Desktop's project path). Its directories and
  rules add to the top-level ones when an engine starts in that directory.
  LynShen Desktop's settings page reads and changes these keys and keeps
  the ones it does not know.
- `always_allow_scope`: where a `project` approval saves its rule:
  `"project"` (the default) or `"global"` (the top-level `command_rules`).

The rules apply in this order: the top-level ones, the project's, then the
session's; `forbid` wins over any other match, otherwise the longest
prefix, `ask` before `allow` at the same length. Engines and the desktop
change config.json one at a time (a lock file, `config.json.lock`, next to
it), so a rule saved by one engine is not lost to another's save.

`/sandbox` shows the current sandbox; `/sandbox <mode>` switches the mode for
the current session. A client changes the session's sandbox with
`set_sandbox`, `add_directory` and `remove_directory`, and makes a directory
writable from an approval with `add_dir` (`docs/serve-protocol.md`).
Desktop, Downloads and Documents are not writable unless you add them;
the approval card offers it the first time the model needs one.

Hook commands are written by you and are not sandboxed.

## Shell

The model's commands run in the shell that `shell` in
`~/.lynshen/config.json` names. The sandbox wraps whichever shell runs.

| `shell` | Commands run in |
| --- | --- |
| `"auto"` (default, also when the key is absent) | Windows: the first installed of Git Bash, PowerShell 7, Windows PowerShell 5.1. macOS and Linux: the login shell (`$SHELL`, else the user database's) when it is zsh, bash or sh; else zsh, bash or sh, whichever is installed (zsh first on macOS, bash first on Linux) |
| `"pwsh"` | PowerShell 7 |
| `"powershell"` | Windows PowerShell 5.1 |
| `"gitbash"` | Git for Windows' `bin\bash.exe` (never WSL's `System32\bash.exe`) |
| `"zsh"`, `"bash"`, `"sh"` | That shell |
| an absolute path | That program; its file name must say which shell it is (`pwsh`, `powershell`, `bash`, `zsh`, `sh`, `dash`, `ksh`) |

A shell that is not installed here, or a value the engine does not know,
falls back to `"auto"`; the `shells` op reports why in `commands.notes`.
The Windows order is `WINDOWS_AUTO_ORDER` in
`crates/agent-core/src/shell.rs`.

Where the engine looks:
- PowerShell 7: `pwsh.exe` on `PATH`, `%ProgramFiles%\PowerShell\7`
  (the installer, winget), `%LOCALAPPDATA%\Microsoft\WindowsApps` (Store and
  MSIX), `%ProgramFiles%\PowerShell\7-preview`, `%USERPROFILE%\.dotnet\tools`.
- Windows PowerShell: `%SystemRoot%\System32\WindowsPowerShell\v1.0`, then
  `PATH`.
- Git Bash: `%ProgramFiles%\Git`, `%ProgramFiles(x86)%\Git`,
  `%LOCALAPPDATA%\Programs\Git`, the Git that `git.exe` on `PATH` belongs to,
  Scoop's.
- zsh, bash, sh: `PATH`, then `/bin`, `/usr/bin`.

How each shell runs a command:
- PowerShell 7 and 5.1: `-NoProfile -ExecutionPolicy Bypass -Command`,
  after a prelude that sets `$OutputEncoding`, `[Console]::OutputEncoding`
  and `[Console]::InputEncoding` to UTF-8 (without a BOM) and turns off
  progress bars. Without it, Chinese text is garbled under a GBK code page.
- Git Bash: `bash.exe -lc`, with `CHERE_INVOKING=1` so that its
  `/etc/profile` stays in the working directory.
- zsh and bash: `-c`, in the login environment (below). Without one,
  `-lc`; when even `-lc` does not start within 5 seconds, `/bin/sh -c`.
- sh: `sh -lc`, as before this setting.

Every command also gets `NO_COLOR=1`, `TERM=dumb`, `PAGER=cat`,
`GIT_PAGER=cat`, and `GIT_TERMINAL_PROMPT=0` unless it is set. POSIX shells
get `LANG=en_US.UTF-8` (macOS) or `C.UTF-8` when none of `LC_ALL`,
`LC_CTYPE` and `LANG` is set. A terminal gets none of these.

### Login environment

Users add to `PATH` in `~/.zshrc` (nvm, pyenv, conda, Homebrew), which a
non-interactive shell does not read. So for zsh and bash the engine starts
the shell once as an interactive login shell, `$SHELL -ilc`, and keeps the
environment it prints (`command env -0`, written to a temporary file and
ended by a marker). Each command then runs with `-c` in that environment;
`-l` again would let macOS' `path_helper` move `/usr/bin` ahead of the
user's directories. `PWD`, `OLDPWD`, `SHLVL` and `_` are left out.

The snapshot runs with input, output and errors closed, in its own process
group, and is killed after 5 seconds: an rc file that waits for input gets
end of file, one that starts a background program cannot hold a pipe open,
and one that hangs or exits early only costs the snapshot. Aliases and
functions from rc files are not kept.

An engine takes the snapshot in the background when it starts, once per
process and setting (the daemon hosts all its sessions in one process).
A new `shell` value, the `shells` op with `refresh: true`, or a restart
takes it again.

### What the model is told about the shell

The system prompt's `<env>` block names the shell and its version, for
example `Commands run in zsh 5.9.` or `Commands run in PowerShell 7.5.2
(pwsh): …`. Windows PowerShell 5.1 adds that it has no `&&` or `||` (use
`;` and `if ($?) { … }`). Git Bash adds that Windows-native work can run
in `pwsh -NoProfile -Command "…"`, with PowerShell's version. When the
shell changes during a session, the model gets a note on its next turn.

### Terminals

A terminal (the daemon's `term_open`, Desktop's terminal panel) opens
`terminal_shell`, else `shell`. `terminal_shell` takes the same values,
and an absolute path to any program (`cmd.exe`, `fish`). `"auto"` is the
model's shell on Windows, and the login shell, whichever it is, on macOS
and Linux. zsh, bash, sh and fish start with `-l`, Git Bash with
`--login -i`, PowerShell with `-NoLogo`.
