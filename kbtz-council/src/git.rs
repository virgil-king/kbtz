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
pub fn shallow_clone(source: &Path, dest: &Path) -> io::Result<()> {
    let output = Command::new("git")
        .args(["clone", "--depth", "1"])
        .arg(source)
        .arg(dest)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git clone failed: {}", stderr),
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

/// Set up a session directory with shallow clones of the specified repos.
/// `repos` is a list of (name, source_path) pairs.
pub fn setup_session_dir(session_dir: &Path, repos: &[(&str, &Path)]) -> io::Result<()> {
    std::fs::create_dir_all(session_dir.join("files"))?;
    for (name, source) in repos {
        shallow_clone(source, &session_dir.join(name))?;
    }
    Ok(())
}

/// Delete a session directory entirely.
pub fn cleanup_session_dir(session_dir: &Path) -> io::Result<()> {
    std::fs::remove_dir_all(session_dir)
}
