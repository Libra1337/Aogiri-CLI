//! Linux only. Each check starts `/bin/sh` with the ruleset applied between
//! fork and exec (`Prepared::apply` allocates nothing for that reason).
//! Without Landlock the checks are skipped, unless `LYNSHEN_SANDBOX_CI` is
//! set (CI runs on a kernel that has it).

use super::*;
use std::{
    os::unix::process::CommandExt,
    process::{Command, Output},
};

fn available() -> bool {
    if abi() > 0 {
        return true;
    }
    assert!(
        std::env::var_os("LYNSHEN_SANDBOX_CI").is_none(),
        "Landlock is not available on this kernel"
    );
    eprintln!("skipped: Landlock is not available on this kernel");
    false
}

fn work(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lynshen-landlock-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn spec(roots: &[&Path], deny_read: &[PathBuf], network: bool) -> Spec {
    Spec {
        writable_roots: roots.iter().map(|root| root.to_path_buf()).collect(),
        deny_read: deny_read.to_vec(),
        network,
        ..Spec::default()
    }
}

fn sh(spec: &Spec, cwd: &Path, program: &str, script: &str) -> Output {
    let prepared = Prepared::new(spec).unwrap();
    let mut command = Command::new(program);
    command.args(["-c", script]).current_dir(cwd);
    // SAFETY: `apply` makes system calls only, as a child between fork and
    // exec must.
    unsafe {
        command.pre_exec(move || prepared.apply());
    }
    command.output().unwrap()
}

#[test]
fn landlock_keeps_writes_inside_the_writable_roots() {
    if !available() {
        return;
    }
    let base = work("writes");
    let ws = base.join("ws");
    let outside = base.join("outside");
    fs::create_dir_all(ws.join("sub")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(ws.join("sub/existing.txt"), "one\n").unwrap();
    let spec = spec(&[&ws], &[], true);
    let ok = |script: &str| sh(&spec, &ws, "/bin/sh", script).status.success();
    assert!(ok(
        "echo ok > inside.txt && mkdir -p a/b && echo x > a/b/c.txt"
    ));
    assert!(ok(
        "echo two >> sub/existing.txt && mv a/b/c.txt sub/ && rm sub/c.txt"
    ));
    assert!(ok("echo quiet > /dev/null"));
    assert!(!ok(&format!("echo no > {}/escape.txt", outside.display())));
    assert!(!outside.join("escape.txt").exists());
    assert!(!ok(&format!("mkdir {}/made", outside.display())));
    // Reading outside is allowed.
    fs::write(outside.join("readable.txt"), "visible").unwrap();
    let read = sh(
        &spec,
        &ws,
        "/bin/sh",
        &format!("cat {}/readable.txt", outside.display()),
    );
    assert_eq!(String::from_utf8_lossy(&read.stdout), "visible");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn landlock_hides_credentials() {
    if !available() {
        return;
    }
    let base = work("reads");
    let ws = base.join("ws");
    let secrets = base.join("profile/.ssh");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("id_test"), "top-secret").unwrap();
    fs::write(base.join("profile/readable.txt"), "visible").unwrap();
    let spec = spec(&[&ws], std::slice::from_ref(&secrets), true);
    let read = sh(
        &spec,
        &ws,
        "/bin/sh",
        &format!("cat {}/id_test", secrets.display()),
    );
    assert!(!read.status.success());
    assert!(!String::from_utf8_lossy(&read.stdout).contains("top-secret"));
    let listing = sh(&spec, &ws, "/bin/sh", &format!("ls {}", secrets.display()));
    assert!(!String::from_utf8_lossy(&listing.stdout).contains("id_test"));
    // Its neighbours stay readable.
    let read = sh(
        &spec,
        &ws,
        "/bin/sh",
        &format!("cat {}/profile/readable.txt", base.display()),
    );
    assert_eq!(String::from_utf8_lossy(&read.stdout), "visible");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn with_the_network_off_no_internet_socket_opens() {
    if !available() || !Path::new("/bin/bash").exists() {
        return;
    }
    let base = work("net");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
    let on = sh(&spec(&[&base], &[], true), &base, "/bin/bash", &connect);
    assert!(
        on.status.success(),
        "{}",
        String::from_utf8_lossy(&on.stderr)
    );
    let off = sh(&spec(&[&base], &[], false), &base, "/bin/bash", &connect);
    assert!(!off.status.success());
    // Local (Unix) sockets still work: pipes and the like are unaffected.
    let local = sh(
        &spec(&[&base], &[], false),
        &base,
        "/bin/sh",
        "echo piped | cat",
    );
    assert_eq!(String::from_utf8_lossy(&local.stdout), "piped\n");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn the_runner_settings_round_trip() {
    let spec = spec(&[Path::new("/w")], &[PathBuf::from("/h/.ssh")], false);
    let back = spec_from_json(&spec_to_json(&spec));
    assert_eq!(back.writable_roots, spec.writable_roots);
    assert_eq!(back.deny_read, spec.deny_read);
    assert!(!back.network);
}
