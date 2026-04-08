use std::process::Command;
use tempfile::TempDir;

fn init_repo(dir: &std::path::Path) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("file.txt"), "hello").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(dir)
        .output()
        .unwrap();
}

#[test]
fn shallow_clone_creates_repo_with_single_commit() {
    let source = TempDir::new().unwrap();
    init_repo(source.path());

    let dest = TempDir::new().unwrap();
    let clone_path = dest.path().join("clone");

    kbtz_council::git::shallow_clone(source.path(), &clone_path).unwrap();

    assert!(clone_path.join("file.txt").exists());
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&clone_path)
        .output()
        .unwrap();
    let lines: Vec<&str> = std::str::from_utf8(&log.stdout)
        .unwrap()
        .trim()
        .lines()
        .collect();
    assert_eq!(lines.len(), 1);
}

#[test]
fn fetch_commits_brings_branch_into_target() {
    let source = TempDir::new().unwrap();
    init_repo(source.path());

    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("clone");
    kbtz_council::git::shallow_clone(source.path(), &clone_path).unwrap();

    // Make a commit in the clone
    std::fs::write(clone_path.join("new.txt"), "new file").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&clone_path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "impl change"])
        .current_dir(&clone_path)
        .output()
        .unwrap();

    // Fetch commits back into source as a named branch
    kbtz_council::git::fetch_branch(source.path(), &clone_path, "step-001").unwrap();

    // Source should now have a step-001 branch
    let branches = Command::new("git")
        .args(["branch", "--list", "step-001"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let output = std::str::from_utf8(&branches.stdout).unwrap().trim();
    assert!(output.contains("step-001"));
}

#[test]
fn setup_session_dir_creates_clones_for_specified_repos() {
    let repo_a = TempDir::new().unwrap();
    init_repo(repo_a.path());
    let repo_b = TempDir::new().unwrap();
    init_repo(repo_b.path());

    let session_dir = TempDir::new().unwrap();
    let repos = vec![
        ("repo-a", repo_a.path()),
        ("repo-b", repo_b.path()),
    ];

    kbtz_council::git::setup_session_dir(session_dir.path(), &repos).unwrap();

    assert!(session_dir.path().join("repo-a/file.txt").exists());
    assert!(session_dir.path().join("repo-b/file.txt").exists());
    assert!(session_dir.path().join("files").is_dir());
}
