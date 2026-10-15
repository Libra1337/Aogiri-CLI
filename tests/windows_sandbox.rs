//! The Windows sandbox end to end (CI: windows-latest): config's
//! `sandbox_windows = "restricted-token"` through `SandboxPolicy::wrap`,
//! the `lynshen __sandbox-exec` runner and the shells the engine uses, then
//! `release_os_sandbox`. Run with `--nocapture`: every check prints.
//!
//! Every path lies in one temporary directory. `HOME`, `USERPROFILE` and
//! `APPDATA` point at a stand-in profile there, so the credential paths the
//! sandbox protects are the test's own.
#![cfg(windows)]

use lynshen_agent_core::{
    sandbox::{self, SandboxMode, SandboxPolicy, WindowsSandbox},
    shell::{CommandShell, Host},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Report(Vec<(String, bool)>);

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        println!(
            "[{}] {name}: {}",
            if ok { "PASS" } else { "FAIL" },
            detail.as_ref()
        );
        self.0.push((name.to_string(), ok));
    }
}

/// Runs `program args` wrapped by `policy`, from `cwd`, as the engine's
/// shell tool does: (success, description).
fn run(
    policy: &SandboxPolicy,
    cwd: &Path,
    program: &str,
    args: &[&str],
    env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> (bool, String, String) {
    let (program, args) = match policy.wrap(program, args, cwd) {
        Ok(wrapped) => wrapped,
        Err(error) => return (false, String::new(), format!("wrap failed: {error}")),
    };
    let output = Command::new(&program)
        .args(&args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = format!(
                "{:?}, stdout {:?}, stderr {:?}",
                output.status.code(),
                stdout.trim(),
                stderr.trim().chars().take(300).collect::<String>()
            );
            (output.status.success(), stdout, detail)
        }
        Err(error) => (false, String::new(), format!("spawn failed: {error}")),
    }
}

fn cmd(policy: &SandboxPolicy, cwd: &Path, script: &str) -> (bool, String, String) {
    run(policy, cwd, "cmd.exe", &["/d", "/c", script], &[])
}

#[test]
fn the_restricted_token_sandbox_runs_the_engine_s_commands() {
    let ci = std::env::var_os("LYNSHEN_SANDBOX_CI").is_some();
    let base = std::env::temp_dir().join(format!("lynshen-sbx-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let profile = base.join("profile");
    let ws = base.join("ws");
    let outside = base.join("outside");
    for dir in [
        profile.join(".ssh"),
        profile.join(".lynshen"),
        profile.join("AppData"),
        ws.clone(),
        outside.clone(),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    let secret = profile.join(".ssh").join("id_test");
    fs::write(&secret, "top-secret").unwrap();
    let auth = profile.join(".lynshen").join("auth.json");
    fs::write(&auth, "{\"token\":\"top-secret\"}").unwrap();
    for (key, value) in [
        ("HOME", &profile),
        ("USERPROFILE", &profile),
        ("APPDATA", &profile.join("AppData")),
    ] {
        std::env::set_var(key, value);
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status()
        .is_ok_and(|status| status.success());
    if !git {
        fs::create_dir_all(ws.join(".git")).unwrap();
    }
    let mut report = Report(Vec::new());

    // Off by default: nothing is wrapped, nothing is enforced.
    let policy = SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        ..SandboxPolicy::default_for_platform()
    };
    report.check(
        "off by default: not enforced and not wrapped",
        !policy.enforced() && policy.wrap("cmd.exe", &["/c", "ver"], &ws).unwrap().0 == "cmd.exe",
        "",
    );

    sandbox::set_windows_sandbox(WindowsSandbox::RestrictedToken);
    sandbox::set_sandbox_runner(PathBuf::from(env!("CARGO_BIN_EXE_lynshen")));
    report.check(
        "restricted-token: enforced and available",
        policy.enforced() && policy.check_available().is_ok(),
        format!("{:?}", policy.check_available()),
    );
    let prompt = policy.prompt();
    report.check(
        "the prompt tells the model what this sandbox does",
        prompt.contains("private temp directory") && prompt.contains("Network is not restricted"),
        &prompt,
    );

    // Files.
    let (ok, _, detail) = cmd(&policy, &ws, "echo ok> inside.txt");
    report.check(
        "write inside the workspace",
        ok && ws.join("inside.txt").exists(),
        detail,
    );
    let escape = outside.join("escape.txt");
    let (ok, _, detail) = cmd(&policy, &ws, &format!("echo no> {}", escape.display()));
    report.check(
        "write outside the workspace fails",
        !ok && !escape.exists(),
        detail,
    );
    let planted = profile.join("planted.txt");
    let (ok, _, detail) = cmd(&policy, &ws, &format!("echo no> {}", planted.display()));
    report.check(
        "write into the profile fails",
        !ok && !planted.exists(),
        detail,
    );
    let (ok, _, detail) = cmd(&policy, &ws, r"echo no> .git\planted");
    report.check(
        "write into .git fails",
        !ok && !ws.join(".git/planted").exists(),
        detail,
    );
    for path in [&secret, &auth] {
        let (_, stdout, detail) = cmd(&policy, &ws, &format!("type {}", path.display()));
        report.check(
            &format!("read {} fails", path.display()),
            !stdout.contains("top-secret"),
            detail,
        );
    }
    let (ok, _, detail) = cmd(&policy, &ws, r"echo t> %TEMP%\probe.txt && echo %TEMP%");
    report.check("the command has a writable TEMP", ok, detail);

    // Network: on by default (the contract), and not restricted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        }
    });
    let url = format!("http://127.0.0.1:{port}/");
    let (ok, stdout, detail) = run(
        &policy,
        &ws,
        "curl.exe",
        &["-s", "--noproxy", "*", "--max-time", "20", &url],
        &[],
    );
    report.check(
        "network: a loopback request works",
        ok && stdout.contains("hello"),
        detail,
    );

    // The engine's shells, as its shell tool starts them.
    let host = Host::current();
    for setting in ["pwsh", "powershell", "gitbash"] {
        let shell = CommandShell::detect(&host, setting);
        if !shell.notes.is_empty() {
            report.check(
                &format!("shell {setting}"),
                !ci,
                format!("not available: {:?}", shell.notes),
            );
            continue;
        }
        let script = if shell.shell.kind.powershell() {
            "Set-Content -Path shell-ok.txt -Value 1; Get-Content shell-ok.txt"
        } else {
            "echo 1 > shell-ok.txt && cat shell-ok.txt"
        };
        let _ = fs::remove_file(ws.join("shell-ok.txt"));
        let invocation = shell.invocation(script);
        let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
        let (ok, stdout, detail) = run(&policy, &ws, &invocation.program, &args, &invocation.env);
        report.check(
            &format!("shell {setting}: runs and writes the workspace"),
            ok && stdout.contains('1') && ws.join("shell-ok.txt").exists(),
            detail,
        );
        let escape = outside.join(format!("{setting}.txt"));
        let script = if shell.shell.kind.powershell() {
            format!("Set-Content -Path '{}' -Value 1", escape.display())
        } else {
            format!(
                "echo 1 > '{}'",
                escape.display().to_string().replace('\\', "/")
            )
        };
        let invocation = shell.invocation(&script);
        let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
        let (ok, _, detail) = run(&policy, &ws, &invocation.program, &args, &invocation.env);
        report.check(
            &format!("shell {setting}: cannot write outside"),
            !ok && !escape.exists(),
            detail,
        );
    }

    // Release: no ledger, temp directory, ACE or label left behind.
    sandbox::release_os_sandbox();
    let state = profile.join(".lynshen").join("sandbox");
    let left: Vec<String> = fs::read_dir(&state)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    report.check(
        "release leaves no ledger or temp directory",
        left.is_empty(),
        format!("{left:?}"),
    );
    let label = |path: &Path| lynshen_sandbox::windows::explicit_label(path);
    report.check(
        "release takes the labels off",
        matches!(label(&ws), Ok(None))
            && matches!(label(&ws.join("inside.txt")), Ok(None))
            && matches!(label(&profile.join(".ssh")), Ok(None))
            && matches!(label(&auth), Ok(None)),
        format!(
            "ws {:?}, inside.txt {:?}, .ssh {:?}, auth.json {:?}",
            label(&ws),
            label(&ws.join("inside.txt")),
            label(&profile.join(".ssh")),
            label(&auth)
        ),
    );

    let _ = fs::remove_dir_all(&base);
    let failed: Vec<&str> = report
        .0
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(failed.is_empty(), "failed: {failed:?}");
}
