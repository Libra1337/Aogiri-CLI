//! Git worktrees the daemon makes for sessions (a scheduled run that works
//! apart from the main checkout). They follow the desktop's parallel-task
//! layout, so its 并行任务 list and Git panel take them as their own: the
//! worktree is `<repo parent>/.lynshen-worktrees/<repo name>/<slug>` on
//! branch `task/<slug>`.

use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

/// The directory beside a repository that holds its task worktrees.
pub const CONTAINER: &str = ".lynshen-worktrees";

/// One worktree made at a time: two at once in a repository race on its
/// lock files.
static CREATING: Mutex<()> = Mutex::new(());

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("git {}: {error}", args.first().unwrap_or(&"")))
    }
}

/// `text` as a worktree slug: lowercase letters, digits and hyphens, no
/// hyphen at either end (the desktop's `is_valid_task_slug`).
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    match slug.trim_matches('-') {
        "" => "run".to_string(),
        slug => slug
            .chars()
            .take(60)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string(),
    }
}

/// `dir` is inside a git repository.
pub fn in_repository(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--show-toplevel"]).is_ok()
}

/// A new worktree for work in `cwd`, from the branch the repository has
/// checked out (its commit, when detached). A taken `slug` gets `-2`,
/// `-3`, ….
///
/// Returns `{repo, path, branch, base, slug}` and the directory to work in:
/// `cwd`'s place inside the new worktree. Uncommitted changes in the main
/// checkout stay there.
pub fn create(cwd: &Path, slug: &str) -> Result<(Value, PathBuf), String> {
    let _one = CREATING.lock().unwrap_or_else(|poison| poison.into_inner());
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("{}: {error}", cwd.display()))?;
    let root = git(&cwd, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        format!(
            "{} 不在 git 仓库里，不能在独立 worktree 中运行",
            cwd.display()
        )
    })?;
    let root = PathBuf::from(root)
        .canonicalize()
        .map_err(|error| format!("repository root: {error}"))?;
    let name = root.file_name().ok_or("repository has no name")?;
    let container = root
        .parent()
        .ok_or("repository has no parent directory")?
        .join(CONTAINER)
        .join(name);
    std::fs::create_dir_all(&container)
        .map_err(|error| format!("{}: {error}", container.display()))?;
    let base = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])? {
        head if head == "HEAD" => git(&root, &["rev-parse", "HEAD"])?,
        branch => branch,
    };
    let taken = |s: &str| {
        container.join(s).exists()
            || git(
                &root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/task/{s}"),
                ],
            )
            .is_ok()
    };
    let slug = (1..=100)
        .map(|n| match n {
            1 => slug.to_string(),
            n => format!("{slug}-{n}"),
        })
        .find(|s| !taken(s))
        .ok_or_else(|| format!("no free worktree name for {slug}"))?;
    let branch = format!("task/{slug}");
    let path = container.join(&slug);
    let path_text = path.display().to_string();
    git(
        &root,
        &["worktree", "add", "-b", &branch, &path_text, &base],
    )?;
    if root.join(".gitmodules").exists() {
        let _ = git(&path, &["submodule", "update", "--init", "--recursive"]);
    }
    let inside = match cwd.strip_prefix(&root) {
        Ok(rel) if !rel.as_os_str().is_empty() => path.join(rel),
        _ => path.clone(),
    };
    let tree = json!({
        "repo": root.display().to_string(),
        "path": path_text,
        "branch": branch,
        "base": base,
        "slug": slug,
    });
    Ok((tree, inside))
}

/// Removes worktree `tree` (as `create` returned it) and its branch when
/// nothing was left in them: no uncommitted or untracked files, no commit
/// past its base. One with work in it stays, as does one git cannot read.
/// Returns whether it was removed.
pub fn remove_if_unused(tree: &Value) -> bool {
    let text = |key: &str| tree[key].as_str().unwrap_or_default();
    let (repo, path, branch, base) = (
        Path::new(text("repo")),
        Path::new(text("path")),
        text("branch"),
        text("base"),
    );
    if branch.is_empty() || base.is_empty() || !path.is_dir() {
        return false;
    }
    // Whatever the repository's settings hide: work inside a submodule, and
    // untracked files, count. `--force` below skips git's own check.
    let clean = git(
        path,
        &[
            "status",
            "--porcelain",
            "--ignore-submodules=none",
            "--untracked-files=all",
        ],
    )
    .is_ok_and(|out| out.is_empty());
    let unmoved = git(repo, &["rev-list", "--count", &format!("{base}..{branch}")])
        .is_ok_and(|count| count == "0");
    if !(clean && unmoved) {
        return false;
    }
    let path_text = path.display().to_string();
    // Checked above; git refuses one with submodules without --force.
    if git(repo, &["worktree", "remove", "--force", &path_text]).is_err() {
        return false;
    }
    let _ = git(repo, &["branch", "-D", branch]);
    true
}

/// `cwd` is in a task worktree of the repository `dir` is in
/// (`<repo's parent>/.lynshen-worktrees/<repo's name>/…`, the repository
/// being `dir` or a folder above it).
pub fn in_worktree_of(cwd: &Path, dir: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let cwd = real(cwd);
    real(dir)
        .ancestors()
        .any(|repo| match (repo.parent(), repo.file_name()) {
            (Some(parent), Some(name)) => cwd.starts_with(parent.join(CONTAINER).join(name)),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("lynshen-wt-{label}-{}", std::process::id()))
            .join("app");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(dir.join("web")).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            git(&dir, args).unwrap();
        }
        std::fs::write(dir.join("web/a.txt"), "a").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-qm", "a"]).unwrap();
        dir
    }

    #[test]
    fn a_slug_is_what_the_desktop_takes() {
        assert_eq!(slug("sch-1A2b · 10-26 09:00"), "sch-1a2b-10-26-09-00");
        assert_eq!(slug("--"), "run");
        assert_eq!(slug("巡检"), "run");
    }

    #[test]
    fn a_worktree_beside_the_repo_is_removed_only_when_unused() {
        let dir = repo("unused");
        assert!(in_repository(&dir.join("web")));
        let (tree, inside) = create(&dir.join("web"), "sch-1").unwrap();
        let root = dir.canonicalize().unwrap();
        let path = root.parent().unwrap().join(".lynshen-worktrees/app/sch-1");
        assert_eq!(tree["path"], path.display().to_string());
        assert_eq!(tree["branch"], "task/sch-1");
        assert_eq!(tree["base"], "main");
        assert_eq!(inside, path.join("web"));
        assert!(in_worktree_of(&inside, &dir));
        assert!(!in_worktree_of(&dir.join("web"), &dir));
        // A second one with the same slug gets a free name.
        let (second, _) = create(&dir, "sch-1").unwrap();
        assert_eq!(second["slug"], "sch-1-2");

        // Untracked work keeps it.
        std::fs::write(path.join("new.txt"), "x").unwrap();
        assert!(!remove_if_unused(&tree));
        // So does a commit.
        git(&path, &["add", "."]).unwrap();
        git(&path, &["commit", "-qm", "x"]).unwrap();
        assert!(!remove_if_unused(&tree));
        assert!(path.is_dir());

        assert!(remove_if_unused(&second));
        assert!(!Path::new(second["path"].as_str().unwrap()).exists());
        assert!(git(
            &dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/task/sch-1-2"
            ]
        )
        .is_err());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn a_folder_outside_git_has_no_worktree() {
        let dir = std::env::temp_dir().join(format!("lynshen-wt-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!in_repository(&dir));
        assert!(create(&dir, "sch-1")
            .unwrap_err()
            .contains("不在 git 仓库里"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_worktree_of_the_main_directory_is_apart_from_it() {
        let main = Path::new("/w/app");
        assert!(in_worktree_of(
            Path::new("/w/.lynshen-worktrees/app/sch-1/web"),
            main
        ));
        assert!(!in_worktree_of(
            Path::new("/w/.lynshen-worktrees/app2/sch-1"),
            main
        ));
        assert!(!in_worktree_of(Path::new("/w/app/web"), main));
        // A project in a folder of the repository, and another folder of it.
        let tree = Path::new("/w/.lynshen-worktrees/repo/sch-1/web");
        assert!(in_worktree_of(tree, Path::new("/w/repo/web")));
        assert!(in_worktree_of(tree, Path::new("/w/repo/shared")));
        assert!(!in_worktree_of(tree, Path::new("/w/other")));
    }
}
