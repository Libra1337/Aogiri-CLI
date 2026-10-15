//! Windows only (CI: windows-latest). One test runs every check in order,
//! prints all results, then fails if one failed: a Windows CI round trip
//! is slow, so each run should show everything. Run it with
//! `--nocapture`. `LYNSHEN_SANDBOX_CI=1` makes a missing tool (git, node,
//! cargo, curl) a failure instead of a skip.
//!
//! Every path the test touches lies in one temporary directory, the
//! "profile" with its credential folder included (`USERPROFILE` points at
//! it), and is deleted at the end.

use super::*;
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Write},
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};

struct Run {
    code: Option<u32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn describe(&self) -> String {
        format!(
            "exit {:?}, stdout {:?}, stderr {:?}",
            self.code,
            self.stdout.trim(),
            self.stderr.trim().chars().take(300).collect::<String>()
        )
    }
}

struct Report {
    results: Vec<(String, bool, String)>,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!("[{}] {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        self.results.push((name.to_string(), ok, detail));
    }

    fn info(&mut self, name: &str, detail: impl Into<String>) {
        println!("[INFO] {name}: {}", detail.into());
    }
}

/// The test's directory, fixed at first use (the runner check below
/// changes `TEMP`).
fn base() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        std::env::temp_dir().join(format!("lynshen-sbx-test-{}", std::process::id()))
    })
}

static OUTPUTS: AtomicUsize = AtomicUsize::new(0);

/// Runs `program args` in `cwd` with a token for `caps`, waiting up to
/// `timeout`, then ending the job.
fn run_with(
    caps: &[Sid],
    low: bool,
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Run, String> {
    run_variant(
        caps,
        low,
        Variant::WriteRestricted,
        cwd,
        program,
        args,
        timeout,
    )
}

/// `run_with` with a token of `variant`.
fn run_variant(
    caps: &[Sid],
    low: bool,
    variant: Variant,
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Run, String> {
    let token = token::token_with(caps, low, variant)?;
    let outputs = base().join("outputs");
    fs::create_dir_all(&outputs).map_err(|e| e.to_string())?;
    let n = OUTPUTS.fetch_add(1, Ordering::Relaxed);
    let out_path = outputs.join(format!("{n}.out"));
    let err_path = outputs.join(format!("{n}.err"));
    let stdin = File::open("NUL").map_err(|e| e.to_string())?;
    let out = File::create(&out_path).map_err(|e| e.to_string())?;
    let err = File::create(&err_path).map_err(|e| e.to_string())?;
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let child = spawn(
        &token,
        OsStr::new(program),
        &args,
        Some(cwd),
        Some([
            stdin.as_raw_handle(),
            out.as_raw_handle(),
            err.as_raw_handle(),
        ]),
    )?;
    let code = child.wait(Some(timeout))?;
    if code.is_none() {
        child.kill();
    }
    drop((stdin, out, err));
    Ok(Run {
        code,
        stdout: fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: fs::read_to_string(&err_path).unwrap_or_default(),
    })
}

fn run(
    backend: &RestrictedToken,
    spec: &Spec,
    cwd: &Path,
    program: &str,
    args: &[&str],
) -> Result<Run, String> {
    let grant = backend.prepare(spec)?;
    run_with(
        &grant.caps,
        backend.low,
        cwd,
        program,
        args,
        Duration::from_secs(90),
    )
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn git_bash() -> Option<PathBuf> {
    [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn spec_for(roots: &[&Path], read_only: &[PathBuf], deny_read: &[PathBuf]) -> Spec {
    Spec {
        writable_roots: roots.iter().map(|root| root.to_path_buf()).collect(),
        read_only: read_only.to_vec(),
        deny_write: Vec::new(),
        deny_read: deny_read.to_vec(),
        network: true,
        private_temp: false,
    }
}

/// Serves one HTTP response on a loopback port; the port.
fn one_shot_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        }
    });
    port
}

#[test]
fn the_restricted_token_sandbox_enforces_the_policy() {
    let ci = std::env::var_os("LYNSHEN_SANDBOX_CI").is_some();
    let base = base().to_path_buf();
    let _ = fs::remove_dir_all(&base);
    let ws = base.join("ws");
    let outside = base.join("outside");
    let state = base.join("state");
    let profile = base.join("profile");
    fs::create_dir_all(ws.join("sub/deep")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(ws.join("sub/deep/existing.txt"), "one\r\n").unwrap();
    // A stand-in profile: the sandbox refuses to grant the profile, and
    // its credential folder gets the no-read-up label.
    std::env::set_var("USERPROFILE", &profile);
    let ssh = profile.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    let secret = ssh.join("id_test");
    fs::write(&secret, "top-secret").unwrap();
    let readable = profile.join("readable.txt");
    fs::write(&readable, "visible").unwrap();
    let profile_target = profile.join("escape.txt");
    let ssh_label_before = explicit_label(&ssh).unwrap();
    // cmd.exe does not read back C-runtime quoting: the paths below go
    // unquoted, so they must not hold spaces.
    assert!(
        !base.display().to_string().contains(' '),
        "{}",
        base.display()
    );

    let backend = RestrictedToken::new(state.clone());
    backend
        .check()
        .expect("a restricted token can be made here");
    // Git: a real repository, made before the sandbox protects .git.
    let git = on_path("git.exe");
    if git {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&ws)
            .status()
            .unwrap();
        assert!(status.success());
    } else {
        fs::create_dir_all(ws.join(".git")).unwrap();
    }
    let spec = spec_for(&[&ws], &[ws.join(".git")], std::slice::from_ref(&ssh));
    let mut report = Report {
        results: Vec::new(),
    };
    let cmd = |report: &mut Report, name: &str, script: &str, expect_ok: bool| -> Run {
        let result = run(&backend, &spec, &ws, "cmd.exe", &["/d", "/c", script]).unwrap();
        report.check(name, result.ok() == expect_ok, result.describe());
        result
    };

    // The profile and a drive root are never granted.
    let refused = backend.prepare(&spec_for(&[&profile], &[], &[]));
    report.check(
        "granting the user profile is refused",
        refused.is_err(),
        format!("{:?}", refused.map(|grant| grant.caps)),
    );

    // Writes.
    let started = Instant::now();
    cmd(
        &mut report,
        "write inside the workspace",
        "echo ok> inside.txt",
        true,
    );
    report.info("first prepare + run", format!("{:?}", started.elapsed()));
    report.check(
        "  file written inside",
        ws.join("inside.txt").exists(),
        "inside.txt",
    );
    cmd(
        &mut report,
        "create a directory and a file in it",
        r"mkdir made && echo kept> made\kept.txt",
        true,
    );
    cmd(
        &mut report,
        "append to a file that existed before the grant",
        r"echo two>> sub\deep\existing.txt",
        true,
    );
    let existing = fs::read_to_string(ws.join("sub/deep/existing.txt")).unwrap_or_default();
    report.check("  appended", existing.contains("two"), existing.clone());
    let escape = outside.join("escape.txt");
    cmd(
        &mut report,
        "write outside the writable roots",
        &format!("echo no> {}", escape.display()),
        false,
    );
    report.check(
        "  nothing written outside",
        !escape.exists(),
        escape.display().to_string(),
    );
    cmd(
        &mut report,
        "write into the user profile",
        &format!("echo no> {}", profile_target.display()),
        false,
    );
    report.check(
        "  nothing written into the profile",
        !profile_target.exists(),
        profile_target.display().to_string(),
    );
    cmd(
        &mut report,
        "write into .git",
        r"echo no> .git\planted",
        false,
    );
    report.check(
        "  nothing planted in .git",
        !ws.join(".git/planted").exists(),
        "",
    );
    cmd(&mut report, "rename .git away", "ren .git gitx", false);
    report.check("  .git still there", ws.join(".git").is_dir(), "");
    cmd(
        &mut report,
        "delete a file in the workspace",
        "del inside.txt",
        true,
    );

    // Reads.
    let read = cmd(
        &mut report,
        "read a credential (the profile's .ssh)",
        &format!("type {}", secret.display()),
        false,
    );
    report.check(
        "  secret not in output",
        !read.stdout.contains("top-secret"),
        read.stdout.clone(),
    );
    let listing = run(
        &backend,
        &spec,
        &ws,
        "cmd.exe",
        &["/d", "/c", &format!("dir /b {}", ssh.display())],
    )
    .unwrap();
    report.check(
        "list the credential folder",
        !listing.stdout.contains("id_test"),
        listing.describe(),
    );
    cmd(
        &mut report,
        "read another file in the profile",
        &format!("type {}", readable.display()),
        true,
    );

    // Network: on (the contract's default), and this backend cannot turn
    // it off.
    if on_path("curl.exe") {
        let port = one_shot_server();
        let url = format!("http://127.0.0.1:{port}/");
        let fetched = run(
            &backend,
            &spec,
            &ws,
            "curl.exe",
            &["-s", "--noproxy", "*", "--max-time", "20", &url],
        )
        .unwrap();
        report.check(
            "network: a loopback HTTP request works",
            fetched.ok() && fetched.stdout.contains("hello"),
            fetched.describe(),
        );
    } else {
        report.check("network: a loopback HTTP request works", !ci, "no curl.exe");
    }

    // Tools.
    for (name, program, args) in [
        ("git status", "git.exe", vec!["status", "--short"]),
        ("node -v", "node.exe", vec!["-v"]),
        ("cargo --version", "cargo.exe", vec!["--version"]),
    ] {
        if !on_path(program) {
            report.check(name, !ci, "not installed");
            continue;
        }
        let result = run(&backend, &spec, &ws, program, &args).unwrap();
        report.check(name, result.ok(), result.describe());
    }

    // The job: a background process outlives its parent, then the job ends.
    fs::write(
        ws.join("grand.cmd"),
        "@ping -n 6 127.0.0.1 > %1.ping\r\n@echo late> %1\r\n",
    )
    .unwrap();
    fs::write(
        ws.join("orphan.cmd"),
        "@start \"\" /b cmd /d /c grand.cmd late-orphan.txt\r\n@exit 0\r\n",
    )
    .unwrap();
    fs::write(
        ws.join("slow.cmd"),
        "@start \"\" /b cmd /d /c grand.cmd late-slow.txt\r\n@ping -n 60 127.0.0.1 > slow.txt\r\n",
    )
    .unwrap();
    let caps = backend.prepare(&spec).unwrap().caps;
    // Timeout: the command still runs after 2 s; ending the job ends it and
    // the process it started.
    let timed = run_with(
        &caps,
        true,
        &ws,
        "cmd.exe",
        &["/d", "/c", "slow.cmd"],
        Duration::from_secs(2),
    )
    .unwrap();
    report.check(
        "a slow command times out",
        timed.code.is_none(),
        timed.describe(),
    );
    std::thread::sleep(Duration::from_secs(8));
    report.check(
        "  its child was killed with it",
        !ws.join("late-slow.txt").exists(),
        "late-slow.txt must not appear",
    );
    // Kill on close: the parent exits at once, the orphan dies with the job.
    let token = sandbox_token(&caps, true).unwrap();
    let child = spawn(
        &token,
        OsStr::new("cmd.exe"),
        &["/d".into(), "/c".into(), "orphan.cmd".into()],
        Some(&ws),
        Some([
            File::open("NUL").unwrap().as_raw_handle(),
            File::create(base.join("orphan.out"))
                .unwrap()
                .as_raw_handle(),
            File::create(base.join("orphan.err"))
                .unwrap()
                .as_raw_handle(),
        ]),
    )
    .unwrap();
    let parent = child.wait(Some(Duration::from_secs(20))).unwrap();
    report.info("orphan parent exit", format!("{parent:?}"));
    drop(child);
    std::thread::sleep(Duration::from_secs(8));
    report.check(
        "closing the job kills a process whose parent exited",
        !ws.join("late-orphan.txt").exists(),
        "late-orphan.txt must not appear",
    );

    // Confined agent: its worktree is writable, the project around it not.
    let project = base.join("project");
    let worktree = project.join(".lynshen/agents/sub");
    fs::create_dir_all(&worktree).unwrap();
    fs::create_dir_all(project.join(".git")).unwrap();
    let parent_spec = spec_for(
        &[&project],
        &[project.join(".git"), project.join(".lynshen")],
        &[],
    );
    let confined_spec = Spec {
        deny_write: vec![project.clone()],
        ..spec_for(&[&worktree], &[], &[])
    };
    let parent_run = |script: &str| {
        run(
            &backend,
            &parent_spec,
            &project,
            "cmd.exe",
            &["/d", "/c", script],
        )
        .unwrap()
    };
    let confined_run = |script: &str| {
        run(
            &backend,
            &confined_spec,
            &worktree,
            "cmd.exe",
            &["/d", "/c", script],
        )
        .unwrap()
    };
    let r = parent_run("echo p> parent.txt");
    report.check("parent writes its project", r.ok(), r.describe());
    let r = parent_run(r"echo p> .lynshen\parent.txt");
    report.check("parent cannot write .lynshen", !r.ok(), r.describe());
    let r = confined_run("echo c> confined.txt");
    report.check("confined agent writes its worktree", r.ok(), r.describe());
    let r = confined_run(&format!("echo c> {}", project.join("escape.txt").display()));
    report.check(
        "confined agent cannot write the project",
        !r.ok() && !project.join("escape.txt").exists(),
        r.describe(),
    );

    // The private temp directory, through the runner's own entry point
    // (it sets TEMP and TMP for the command). Exit codes only: the runner
    // passes this process's output handles on.
    let temp_spec = Spec {
        private_temp: true,
        ..spec.clone()
    };
    let grant = backend.prepare(&temp_spec).unwrap();
    let temp = grant.temp.clone().expect("a private temp directory");
    let saved_temp = (std::env::var_os("TEMP"), std::env::var_os("TMP"));
    let settings = serde_json::json!({
        "caps": grant.caps.iter().map(Sid::to_string_sid).collect::<Vec<_>>(),
        "low": true,
        "temp": temp.display().to_string(),
    });
    let previous_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&ws).unwrap();
    let code = run_command(
        &settings,
        &[
            "cmd.exe".into(),
            "/d".into(),
            "/c".into(),
            r"echo t> %TEMP%\probe.txt".into(),
        ],
    );
    std::env::set_current_dir(previous_dir).unwrap();
    for (key, value) in [("TEMP", saved_temp.0), ("TMP", saved_temp.1)] {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    report.check(
        "the runner gives the command a writable private TEMP",
        code == Ok(0) && temp.join("probe.txt").exists(),
        format!("{code:?}, {}", temp.display()),
    );

    // Compatibility, Low against Medium integrity (Medium cannot keep
    // credentials unreadable; this shows what Low costs).
    let compat_caps = grant.caps.clone();
    let mut probes: Vec<(String, String, Vec<String>)> = vec![
        (
            "cmd: write to NUL".into(),
            "cmd.exe".into(),
            vec!["/d".into(), "/c".into(), "echo x > NUL".into()],
        ),
        (
            "pwsh".into(),
            "pwsh.exe".into(),
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ok; Set-Content -Path pw.txt -Value 1".into(),
            ],
        ),
        (
            "powershell 5.1".into(),
            "powershell.exe".into(),
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ok; Set-Content -Path ps.txt -Value 1".into(),
            ],
        ),
        (
            "node: write a file".into(),
            "node.exe".into(),
            vec![
                "-e".into(),
                "require('fs').writeFileSync('n.txt','x');console.log('ok')".into(),
            ],
        ),
        (
            "npm -v".into(),
            "cmd.exe".into(),
            vec!["/d".into(), "/c".into(), "npm -v".into()],
        ),
        (
            "python --version".into(),
            "python.exe".into(),
            vec!["--version".into()],
        ),
        (
            "git status".into(),
            "git.exe".into(),
            vec!["status".into(), "--short".into()],
        ),
        (
            "cargo --version".into(),
            "cargo.exe".into(),
            vec!["--version".into()],
        ),
        (
            "rustc --version".into(),
            "rustc.exe".into(),
            vec!["--version".into()],
        ),
    ];
    if let Some(bash) = git_bash() {
        probes.push((
            "Git Bash".into(),
            bash.display().to_string(),
            vec![
                "-lc".into(),
                "echo ok; touch bash.txt; ls >/dev/null; uname -s".into(),
            ],
        ));
    }
    for (name, program, args) in &probes {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        for low in [true, false] {
            let level = if low { "low" } else { "medium" };
            match run_with(
                &compat_caps,
                low,
                &ws,
                program,
                &args,
                Duration::from_secs(90),
            ) {
                Ok(result) => report.info(&format!("compat {level}: {name}"), result.describe()),
                Err(error) => report.info(&format!("compat {level}: {name}"), error),
            }
        }
    }
    // Git Bash (MSYS2) against the token's parts: which one stops it.
    if let Some(bash) = git_bash() {
        let program = bash.display().to_string();
        for (name, low, variant) in [
            ("low, not write-restricted", true, Variant::Unrestricted),
            ("medium, not write-restricted", false, Variant::Unrestricted),
            (
                "low, write-restricted with the user SID",
                true,
                Variant::WithUser,
            ),
        ] {
            match run_variant(
                &compat_caps,
                low,
                variant,
                &ws,
                &program,
                &["-lc", "echo ok; touch bash-variant.txt; uname -s"],
                Duration::from_secs(90),
            ) {
                Ok(result) => report.info(&format!("Git Bash, {name}"), result.describe()),
                Err(error) => report.info(&format!("Git Bash, {name}"), error),
            }
        }
    }

    // Under Medium the same credential is readable: the label is what
    // stops it.
    let medium_read = run_with(
        &compat_caps,
        false,
        &ws,
        "cmd.exe",
        &["/d", "/c", &format!("type {}", secret.display())],
        Duration::from_secs(30),
    )
    .unwrap();
    report.check(
        "the same read works at Medium integrity (the label is what stops it)",
        medium_read.stdout.contains("top-secret"),
        medium_read.describe(),
    );

    // Cost: granting a tree of 20 000 files.
    let tree = base.join("tree");
    for dir in 0..1000 {
        let dir = tree.join(format!("d{dir}"));
        fs::create_dir_all(&dir).unwrap();
        for file in 0..20 {
            fs::write(dir.join(format!("f{file}.txt")), "x").unwrap();
        }
    }
    let started = Instant::now();
    backend.prepare(&spec_for(&[&tree], &[], &[])).unwrap();
    report.info(
        "grant a 20 000-file tree",
        format!("{:?}", started.elapsed()),
    );
    let r = run(
        &backend,
        &spec_for(&[&tree], &[], &[]),
        &tree,
        "cmd.exe",
        &["/d", "/c", r"echo y>> d999\f19.txt"],
    )
    .unwrap();
    report.check(
        "write a file deep in the granted tree",
        r.ok(),
        r.describe(),
    );

    // Cleanup takes every ACE and label off again.
    let ws_sid = root_sid(&ws).expect("the workspace was granted");
    let tree_sid = root_sid(&tree).expect("the tree was granted");
    let started = Instant::now();
    let cleaned = cleanup();
    report.info(
        "cleanup",
        format!("{:?} in {:?}", cleaned, started.elapsed()),
    );
    report.check("cleanup succeeded", cleaned.is_ok(), format!("{cleaned:?}"));
    report.check(
        "  no ACE left on the workspace",
        explicit_ace(&ws, &ws_sid).unwrap().is_none(),
        "",
    );
    report.check(
        "  no deny ACE left on .git",
        explicit_ace(&ws.join(".git"), &ws_sid).unwrap().is_none(),
        format!("{:?}", explicit_ace(&ws.join(".git"), &ws_sid)),
    );
    report.check(
        "  no ACE left deep in the tree",
        explicit_ace(&tree.join("d999/f19.txt"), &tree_sid)
            .unwrap()
            .is_none()
            && explicit_label(&tree).unwrap().is_none(),
        "",
    );
    report.check(
        "  the workspace has no label of ours",
        explicit_label(&ws).unwrap().is_none(),
        format!("{:?}", explicit_label(&ws)),
    );
    let made = [ws.join("made"), ws.join("made/kept.txt")];
    report.check(
        "  what the command created has no label of its own",
        made.iter()
            .all(|path| matches!(explicit_label(path), Ok(None))),
        format!(
            "{:?}",
            made.iter()
                .map(|path| explicit_label(path))
                .collect::<Vec<_>>()
        ),
    );
    report.check(
        "  the credential folder has its label back",
        explicit_label(&ssh).unwrap() == ssh_label_before,
        format!("{:?}", explicit_label(&ssh)),
    );
    report.check(
        "  the private temp directory is gone",
        !temp.exists(),
        temp.display().to_string(),
    );
    report.check(
        "  no ledger left",
        fs::read_dir(&state)
            .map(|dir| dir.count() == 0)
            .unwrap_or(true),
        "",
    );

    let _ = fs::remove_dir_all(&base);
    let failed: Vec<&str> = report
        .results
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert!(failed.is_empty(), "failed: {failed:?}");
}
