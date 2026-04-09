use std::io;
use std::path::Path;
use std::process::Command;

fn run_git(dir: &Path, args: &[&str]) -> io::Result<()> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git {:?} failed: {}", args, stderr),
        ));
    }
    Ok(())
}

/// Create a shallow clone (depth=1) of source repo into dest.
/// If `branch` is provided, clone that specific branch.
pub fn shallow_clone(source: &Path, dest: &Path, branch: Option<&str>) -> io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    let output = cmd.arg(source).arg(dest).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git clone failed: {}", stderr),
        ));
    }
    Ok(())
}

/// Ensure the pool clone at `pool_dir` exists and has the given branch.
/// - If the pool clone doesn't exist, shallow-clone from `source_url` with the branch.
/// - If it exists but doesn't have the branch, fetch it shallowly.
pub fn ensure_pool_branch(
    pool_dir: &Path,
    source_url: &str,
    branch: Option<&str>,
) -> io::Result<()> {
    if !pool_dir.exists() {
        shallow_clone(Path::new(source_url), pool_dir, branch)?;
    } else if let Some(b) = branch {
        // Check if branch already exists locally
        let check = Command::new("git")
            .args(["rev-parse", "--verify", b])
            .current_dir(pool_dir)
            .output()?;
        if !check.status.success() {
            // Fetch the branch shallowly
            run_git(
                pool_dir,
                &["fetch", "--depth", "1", "origin", b],
            )?;
            run_git(pool_dir, &["branch", b, "FETCH_HEAD"])?;
        }
    }
    Ok(())
}

/// Clone from the pool into a session directory.
/// This is a local clone (fast, no network).
pub fn clone_from_pool(
    pool_dir: &Path,
    dest: &Path,
    branch: Option<&str>,
) -> io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["clone"]);
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    let output = cmd.arg(pool_dir).arg(dest).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git clone from pool failed: {}", stderr),
        ));
    }
    Ok(())
}

/// Fetch the current branch from a clone into the target repo as a named branch.
pub fn fetch_branch(target: &Path, clone: &Path, branch_name: &str) -> io::Result<()> {
    let clone_str = clone
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF8 path"))?;

    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(clone)
        .output()?;
    let clone_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

    run_git(
        target,
        &[
            "fetch",
            clone_str,
            &format!("{}:{}", clone_branch, branch_name),
        ],
    )
}

/// Set up a session directory with clones from the pool.
/// `repos` is a list of (name, pool_path, branch) tuples.
pub fn setup_session_dir(
    session_dir: &Path,
    repos: &[(&str, &Path, Option<&str>)],
) -> io::Result<()> {
    std::fs::create_dir_all(session_dir.join("files"))?;
    for (name, pool_path, branch) in repos {
        clone_from_pool(pool_path, &session_dir.join(name), *branch)?;
    }
    Ok(())
}

/// Delete a session directory entirely.
pub fn cleanup_session_dir(session_dir: &Path) -> io::Result<()> {
    std::fs::remove_dir_all(session_dir)
}
