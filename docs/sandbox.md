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
`~/.lynshen/auth.json` and `~/.lynshen/daemon` are unreadable. Network access
is on unless `sandbox_network` is false.

Windows has no OS sandbox yet and defaults to `full-access`. A mode other
than `full-access` (LynShen Desktop's 全自动 sets `workspace-write`) is
accepted there but not enforced: commands run with your permissions, the
model is told the working directory is its boundary and declares crossing
it (`escalate`), and clients see `enforced: false` in the `sandbox` event.
A daemon agent still refuses to start with a sandbox Windows cannot
enforce.

On Linux without `bwrap`, or where AppArmor blocks it (Ubuntu 24.04 and
later restrict unprivileged user namespaces), LynShen reports the sandbox as
unavailable at startup (`unavailable` in the `sandbox` event, with the fix)
and shell commands fail until you install bubblewrap, allow the namespaces
(`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, or an
AppArmor profile for bwrap) or switch to `full-access`; it never falls back
to running commands unsandboxed.

## Approvals

A command inside the sandbox needs no approval, except in `manual` mode,
which still asks for every command. A command that must leave the sandbox
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

`forbid` wins over other matches, otherwise the longest prefix applies. The
defaults let `git add` and `git commit` leave the sandbox and always ask
before `git push`.

A command line is matched part by part. It is split on `&&`, `||`, `;`,
`|`, `&`, new lines and command substitutions (`$(…)`, backticks); code
given inline to a shell (`bash -c "…"`, `powershell -Command …`,
`cmd /c …`) is split the same way. From each part, leading variable
assignments and wrappers come off (`sudo`, `doas`, `env`, `command`,
`time`, `nohup`, `nice`, `timeout`, `xargs`), and a rule matches the part
as written, without its wrappers, or with the program named by its file
name (`/usr/bin/git push` matches `git push`). Then:

- a part that is forbidden makes the whole command forbidden;
- a part that must ask makes the whole command ask;
- the command is allowed only when every part is, except parts that only
  move around, print or filter (`cd`, `echo`, `grep`, `head`, `tail`, ...,
  without a redirection).

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
  `Remove-Item`, ...);
- changing system settings: services and startup items (`launchctl`,
  `systemctl enable|start|stop|…`, `service … start`, `sc create|config|…`,
  `New-Service`, `schtasks /create`, `crontab`), the registry under HKLM,
  the firewall and antivirus (`netsh advfirewall`, `ufw`, `iptables`,
  `pfctl`, `Set-MpPreference`, ...), `spctl`, `csrutil`,
  `Set-ExecutionPolicy`;
- reading or writing credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.lynshen/auth.json`, `~/.lynshen/daemon`).

Never runs:

- deleting the root, the home directory, a drive root, or a directory
  that contains the workspace (`rm -rf ~`, `rm -rf /*`, `rmdir ..`).

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
always ask or never run. A change (`/sandbox`, `set_sandbox`, a directory
added) reaches the model as a note on its next turn. When a sandboxed
command fails with "Operation not permitted", `EPERM` or "Read-only file
system", its result carries a `sandbox_hint` telling the model to run it
again with `escalate` if it must write outside.

### How far an approval reaches

A client answers a request once, for the session, or for the project
(`approve` with `scope`, `docs/serve-protocol.md`):

- **session**: for a command, commands that start with the same words
  (`npm install`, the first two words without `sudo`, `env` and similar
  wrappers) run without asking for the rest of the session, inside or
  outside the sandbox. For a file edit or another tool, that tool stops
  asking. The shell is never allowed as a whole.
- **project**: the prefix is saved as an `allow` rule for the project in
  `~/.lynshen/config.json` (`projects`, see "Settings"), or for every
  project with `always_allow_scope: "global"`. Nothing is written into the
  repository.

Only once, never for the session or the project: requests that must be
asked, interpreters (`python`, `node`, `bash -c`, `powershell -Command`,
...), deletes (`rm`, `Remove-Item`, ...), elevation (`sudo`, `runas`, ...),
`git push --force`, and a command whose parts start differently
(`make && npm test`).

## File tools

The file tools run in-process, so the OS sandbox does not apply to them;
they check paths themselves.

- **Reading** (`read`, `ls`, `ripgrep`, `outline`, image inputs): anywhere,
  except credentials: `~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.lynshen/auth.json`, `~/.lynshen/daemon` (and `%APPDATA%\gnupg` on
  Windows).
- **Writing** (`write`, `edit`, `hashline_edit`, `generate_image`): inside
  the workspace, the read-write directories, temp and the package caches,
  with `.git`, `.lynshen` and `.agents` read-only and nothing writable in
  `read-only`. A write anywhere else is an `outside_write` approval: asked
  in `manual`, `auto-edit` and `plan`, put to the safety model first in
  `auto`, and run without asking in `full-access`, which reports it
  (`escalated`). A write into a credentials directory always asks. It is
  approved once at a time; to stop asking, make the directory writable
  (`approve` with `add_dir`, or `add_directory`).
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
session's; `forbid` wins over any other match, otherwise the longest prefix.

`/sandbox` shows the current sandbox; `/sandbox <mode>` switches the mode for
the current session. A client changes the session's sandbox with
`set_sandbox`, `add_directory` and `remove_directory`, and makes a directory
writable from an approval with `add_dir` (`docs/serve-protocol.md`).
Desktop, Downloads and Documents are not writable unless you add them;
the approval card offers it the first time the model needs one.

Hook commands are written by you and are not sandboxed.
