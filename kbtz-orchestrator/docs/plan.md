# kbtz-orchestrator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a standalone leader-driven AI agent orchestrator that manages implementation steps with stakeholder feedback loops.

**Architecture:** A persistent Rust binary that owns the orchestration event loop. Spawns Claude Code sessions (`claude -p`) for headless execution, embeds an interactive leader session via PTY. Communicates with the leader via an MCP server exposing project management tools. All state on disk in a project directory.

**Tech Stack:** Rust, ratatui/crossterm (TUI), portable-pty (leader PTY), serde/serde_json (serialization), clap (CLI). No async runtime — synchronous + threads, matching kbtz conventions.

---

## File Structure

```
kbtz-orchestrator/
  Cargo.toml
  docs/
    design.md              # Existing design spec
    plan.md                # This file
  src/
    main.rs                # CLI entry point, app bootstrap
    project.rs             # Project state types, file I/O, state snapshot assembly
    step.rs                # Step types, state enum, transitions
    git.rs                 # Shallow clone, fetch commits, branch management
    stream.rs              # stream-json event parser
    session.rs             # Session spawning, lifecycle, process management
    mcp.rs                 # MCP server (stdio transport) for leader tools
    lifecycle.rs           # Pure orchestrator state machine (tick function)
    prompt.rs              # System prompts for leader, implementors, stakeholders
    tui/
      mod.rs               # TUI app state, event loop, view switching
      dashboard.rs         # Dashboard panel: step list, session list, controls
      stream_view.rs       # Stream-json viewer panel (read-only agent output)
      leader.rs            # Interactive leader PTY embedding
  tests/
    lifecycle_test.rs      # State machine unit tests
    git_test.rs            # Git operation tests
    stream_test.rs         # stream-json parser tests
    project_test.rs        # State serialization round-trip tests
```

---

### Task 1: Data Types — Project, Step, Stakeholder

**Files:**
- Modify: `Cargo.toml` (add dependencies)
- Create: `src/project.rs`
- Create: `src/step.rs`
- Modify: `src/main.rs`

- [ ] **Step 0: Set up Cargo.toml dependencies**

Replace `kbtz-orchestrator/Cargo.toml`:

```toml
[package]
name = "kbtz-orchestrator"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tempfile = "3"
```

Additional dependencies (clap, ratatui, crossterm, portable-pty, etc.) will
be added in later tasks as needed.

- [ ] **Step 1: Write tests for project state serialization**

Create `tests/project_test.rs`:

```rust
use kbtz_orchestrator::project::{Project, Stakeholder, RepoConfig};
use kbtz_orchestrator::step::{Step, StepPhase, Dispatch, Feedback, Decision};

#[test]
fn project_state_round_trip() {
    let project = Project {
        repos: vec![
            RepoConfig { name: "backend".into(), url: "/home/user/backend".into() },
            RepoConfig { name: "frontend".into(), url: "/home/user/frontend".into() },
        ],
        stakeholders: vec![
            Stakeholder { name: "security".into(), persona: "Review for auth and injection vulnerabilities.".into() },
            Stakeholder { name: "api-design".into(), persona: "Review for REST conventions and backwards compatibility.".into() },
        ],
        goal_summary: "Add user authentication to the API".into(),
    };

    let json = serde_json::to_string_pretty(&project).unwrap();
    let parsed: Project = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.repos.len(), 2);
    assert_eq!(parsed.stakeholders[0].name, "security");
    assert_eq!(parsed.goal_summary, "Add user authentication to the API");
}

#[test]
fn step_state_round_trip() {
    let step = Step {
        id: "step-001".into(),
        phase: StepPhase::Dispatched,
        dispatch: Dispatch {
            prompt: "Add JWT auth middleware".into(),
            repos: vec!["backend".into()],
            files: vec![],
        },
        summary: None,
        feedback: vec![],
        decision: None,
    };

    let json = serde_json::to_string_pretty(&step).unwrap();
    let parsed: Step = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "step-001");
    assert!(matches!(parsed.phase, StepPhase::Dispatched));
}

#[test]
fn step_phases_serialize_as_lowercase() {
    let phases = vec![
        StepPhase::Dispatched,
        StepPhase::Running,
        StepPhase::Completed,
        StepPhase::Reviewing,
        StepPhase::Reviewed,
        StepPhase::Merged,
        StepPhase::Rework,
    ];
    for phase in phases {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: StepPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(std::mem::discriminant(&phase), std::mem::discriminant(&parsed));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: FAIL — modules don't exist yet.

- [ ] **Step 3: Implement project types**

Create `src/project.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stakeholder {
    pub name: String,
    pub persona: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub repos: Vec<RepoConfig>,
    pub stakeholders: Vec<Stakeholder>,
    pub goal_summary: String,
}
```

- [ ] **Step 4: Implement step types**

Create `src/step.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Dispatched,
    Running,
    Completed,
    Reviewing,
    Reviewed,
    Merged,
    Rework,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispatch {
    pub prompt: String,
    pub repos: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub stakeholder: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Merge,
    Rework { feedback: String },
    Abandon,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub phase: StepPhase,
    pub dispatch: Dispatch,
    pub summary: Option<String>,
    pub feedback: Vec<Feedback>,
    pub decision: Option<Decision>,
}
```

- [ ] **Step 5: Wire up lib.rs and main.rs**

Replace `src/main.rs`:

```rust
fn main() {
    println!("kbtz-orchestrator");
}
```

Create `src/lib.rs`:

```rust
pub mod project;
pub mod step;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: PASS — all 3 tests.

- [ ] **Step 7: Commit**

```bash
git add kbtz-orchestrator/src/ kbtz-orchestrator/tests/ kbtz-orchestrator/Cargo.toml
git commit -m "feat(orchestrator): add project and step data types"
```

---

### Task 2: Project Directory I/O

**Files:**
- Modify: `src/project.rs`
- Create: `tests/project_io_test.rs`

- [ ] **Step 1: Write tests for project directory operations**

Add to `tests/project_test.rs`:

```rust
use std::fs;
use tempfile::TempDir;
use kbtz_orchestrator::project::{Project, Stakeholder, RepoConfig, ProjectDir};

#[test]
fn project_dir_init_creates_structure() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into() }],
        stakeholders: vec![
            Stakeholder { name: "security".into(), persona: "Check auth".into() },
        ],
        goal_summary: "Test project".into(),
    };

    let dir = ProjectDir::init(tmp.path(), &project).unwrap();

    assert!(dir.root().join("state.json").exists());
    assert!(dir.root().join("repos").is_dir());
    assert!(dir.root().join("steps").is_dir());
    assert!(dir.root().join("sessions").is_dir());
    assert!(dir.root().join("claude-sessions").is_dir());
}

#[test]
fn project_dir_load_reads_state() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into() }],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().project.goal_summary, "Test");
}

#[test]
fn state_tracks_steps() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let mut dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let step_id = dir.add_step(Dispatch {
        prompt: "Do the thing".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();

    assert_eq!(step_id, "step-001");
    assert_eq!(dir.state().steps.len(), 1);

    // Persists across reload
    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().steps.len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: FAIL — `ProjectDir` doesn't exist.

- [ ] **Step 3: Implement ProjectDir**

Add to `src/project.rs`:

```rust
use crate::step::{Dispatch, Step, StepPhase};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorState {
    pub project: Project,
    pub steps: Vec<Step>,
    pub next_step_id: u32,
}

pub struct ProjectDir {
    root: PathBuf,
    state: OrchestratorState,
}

impl ProjectDir {
    pub fn init(root: &Path, project: &Project) -> std::io::Result<Self> {
        fs::create_dir_all(root.join("repos"))?;
        fs::create_dir_all(root.join("steps"))?;
        fs::create_dir_all(root.join("sessions"))?;
        fs::create_dir_all(root.join("claude-sessions"))?;

        let state = OrchestratorState {
            project: project.clone(),
            steps: vec![],
            next_step_id: 1,
        };

        let dir = Self {
            root: root.to_path_buf(),
            state,
        };
        dir.save()?;
        Ok(dir)
    }

    pub fn load(root: &Path) -> std::io::Result<Self> {
        let data = fs::read_to_string(root.join("state.json"))?;
        let state: OrchestratorState = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            root: root.to_path_buf(),
            state,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> &OrchestratorState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut OrchestratorState {
        &mut self.state
    }

    pub fn add_step(&mut self, dispatch: Dispatch) -> std::io::Result<String> {
        let id = format!("step-{:03}", self.state.next_step_id);
        self.state.next_step_id += 1;

        let step = Step {
            id: id.clone(),
            phase: StepPhase::Dispatched,
            dispatch,
            summary: None,
            feedback: vec![],
            decision: None,
        };

        // Create step directory
        let step_dir = self.root.join("steps").join(&id);
        fs::create_dir_all(step_dir.join("feedback"))?;

        // Write dispatch.json
        let dispatch_json = serde_json::to_string_pretty(&step.dispatch)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(step_dir.join("dispatch.json"), dispatch_json)?;

        self.state.steps.push(step);
        self.save()?;
        Ok(id)
    }

    fn save(&self) -> std::io::Result<()> {
        let data = serde_json::to_string_pretty(&self.state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(self.root.join("state.json"), data)?;
        Ok(())
    }

    pub fn persist(&self) -> std::io::Result<()> {
        self.save()
    }
}
```

- [ ] **Step 4: Add `use` for `Dispatch` in test file, run tests**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: PASS — all tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add ProjectDir for state persistence"
```

---

### Task 3: Git Operations — Clone and Fetch

**Files:**
- Create: `src/git.rs`
- Create: `tests/git_test.rs`

- [ ] **Step 1: Write tests for git operations**

Create `tests/git_test.rs`:

```rust
use std::process::Command;
use tempfile::TempDir;
use kbtz_orchestrator::git;

fn init_repo(dir: &std::path::Path) {
    Command::new("git").args(["init"]).current_dir(dir).output().unwrap();
    Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(dir).output().unwrap();
    Command::new("git").args(["config", "user.name", "Test"]).current_dir(dir).output().unwrap();
    std::fs::write(dir.join("file.txt"), "hello").unwrap();
    Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
    Command::new("git").args(["commit", "-m", "initial"]).current_dir(dir).output().unwrap();
}

#[test]
fn shallow_clone_creates_repo_with_single_commit() {
    let source = TempDir::new().unwrap();
    init_repo(source.path());

    let dest = TempDir::new().unwrap();
    let clone_path = dest.path().join("clone");

    git::shallow_clone(source.path(), &clone_path).unwrap();

    assert!(clone_path.join("file.txt").exists());
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&clone_path)
        .output().unwrap();
    let lines: Vec<&str> = std::str::from_utf8(&log.stdout).unwrap().trim().lines().collect();
    assert_eq!(lines.len(), 1);
}

#[test]
fn fetch_commits_brings_branch_into_target() {
    let source = TempDir::new().unwrap();
    init_repo(source.path());

    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("clone");
    git::shallow_clone(source.path(), &clone_path).unwrap();

    // Make a commit in the clone
    std::fs::write(clone_path.join("new.txt"), "new file").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&clone_path).output().unwrap();
    Command::new("git").args(["commit", "-m", "impl change"]).current_dir(&clone_path).output().unwrap();

    // Fetch commits back into source as a named branch
    git::fetch_branch(source.path(), &clone_path, "step-001").unwrap();

    // Source should now have a step-001 branch
    let branches = Command::new("git")
        .args(["branch", "--list", "step-001"])
        .current_dir(source.path())
        .output().unwrap();
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

    git::setup_session_dir(session_dir.path(), &repos).unwrap();

    assert!(session_dir.path().join("repo-a/file.txt").exists());
    assert!(session_dir.path().join("repo-b/file.txt").exists());
    assert!(session_dir.path().join("files").is_dir());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: FAIL — `git` module doesn't exist.

- [ ] **Step 3: Implement git operations**

Create `src/git.rs`:

```rust
use std::path::Path;
use std::process::Command;
use std::io;

fn run_git(dir: &Path, args: &[&str]) -> io::Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(io::ErrorKind::Other, format!("git {:?} failed: {}", args, stderr)));
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
        return Err(io::Error::new(io::ErrorKind::Other, format!("git clone failed: {}", stderr)));
    }
    Ok(())
}

/// Fetch the current branch from a clone into the target repo as a named branch.
pub fn fetch_branch(target: &Path, clone: &Path, branch_name: &str) -> io::Result<()> {
    let clone_str = clone.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF8 path"))?;

    // Get the current branch name in the clone
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(clone)
        .output()?;
    let clone_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

    run_git(target, &["fetch", clone_str, &format!("{}:{}", clone_branch, branch_name)])
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
```

- [ ] **Step 4: Add `pub mod git;` to lib.rs, run tests**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add git operations for clone and fetch"
```

---

### Task 4: Stream-JSON Parser

**Files:**
- Create: `src/stream.rs`
- Create: `tests/stream_test.rs`

Claude Code's `--output-format stream-json` emits newline-delimited JSON objects. Each has a `type` field. Key types for our purposes: `assistant`, `tool_use`, `tool_result`, `result`. The `assistant` type contains `message.content` with text/thinking blocks. The `result` type has the final output.

- [ ] **Step 1: Write tests for stream-json parsing**

Create `tests/stream_test.rs`:

```rust
use kbtz_orchestrator::stream::{StreamEvent, parse_stream_line};

#[test]
fn parse_assistant_text_event() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::AssistantText(text) => assert_eq!(text, "Hello world"),
        other => panic!("expected AssistantText, got {:?}", other),
    }
}

#[test]
fn parse_thinking_event() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me analyze..."}]}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::Thinking(text) => assert_eq!(text, "Let me analyze..."),
        other => panic!("expected Thinking, got {:?}", other),
    }
}

#[test]
fn parse_tool_use_event() {
    let line = r#"{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/foo.txt"}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::ToolUse { name, input } => {
            assert_eq!(name, "Read");
            assert!(input.contains("foo.txt"));
        }
        other => panic!("expected ToolUse, got {:?}", other),
    }
}

#[test]
fn parse_result_event() {
    let line = r#"{"type":"result","result":"Done with the task","cost_usd":0.05,"duration_ms":12000}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::Result { result } => assert_eq!(result, "Done with the task"),
        other => panic!("expected Result, got {:?}", other),
    }
}

#[test]
fn parse_unknown_type_returns_other() {
    let line = r#"{"type":"system","message":"starting"}"#;
    let event = parse_stream_line(line).unwrap();
    assert!(matches!(event, StreamEvent::Other(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: FAIL.

- [ ] **Step 3: Implement stream-json parser**

Create `src/stream.rs`:

```rust
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    AssistantText(String),
    Thinking(String),
    ToolUse { name: String, input: String },
    ToolResult { content: String },
    Result { result: String },
    Other(String),
}

pub fn parse_stream_line(line: &str) -> Result<StreamEvent, serde_json::Error> {
    let v: Value = serde_json::from_str(line)?;
    let event_type = v["type"].as_str().unwrap_or("");

    match event_type {
        "assistant" => {
            let content = &v["message"]["content"];
            if let Some(arr) = content.as_array() {
                for item in arr {
                    match item["type"].as_str().unwrap_or("") {
                        "thinking" => {
                            if let Some(text) = item["thinking"].as_str() {
                                return Ok(StreamEvent::Thinking(text.to_string()));
                            }
                        }
                        "text" => {
                            if let Some(text) = item["text"].as_str() {
                                return Ok(StreamEvent::AssistantText(text.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(StreamEvent::Other(line.to_string()))
        }
        "tool_use" => {
            let name = v["name"].as_str().unwrap_or("unknown").to_string();
            let input = v["input"].to_string();
            Ok(StreamEvent::ToolUse { name, input })
        }
        "tool_result" => {
            let content = v["content"].as_str().unwrap_or("").to_string();
            Ok(StreamEvent::ToolResult { content })
        }
        "result" => {
            let result = v["result"].as_str().unwrap_or("").to_string();
            Ok(StreamEvent::Result { result })
        }
        _ => Ok(StreamEvent::Other(line.to_string())),
    }
}
```

- [ ] **Step 4: Add `pub mod stream;` to lib.rs, run tests**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add stream-json event parser"
```

---

### Task 5: Session Runner

**Files:**
- Create: `src/session.rs`

This module spawns `claude -p` processes, reads their stream-json output, and tracks their lifecycle. It does NOT manage step state transitions — that's the lifecycle module's job.

- [ ] **Step 1: Implement session types and spawning**

Create `src/session.rs`:

```rust
use crate::stream::{parse_stream_line, StreamEvent};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub struct SessionId(pub String);

#[derive(Debug)]
pub enum SessionMessage {
    Event(StreamEvent),
    RawLine(String),
    Exited { code: Option<i32> },
}

/// A running headless Claude Code session.
pub struct HeadlessSession {
    pub id: SessionId,
    pub step_id: String,
    pub role: SessionRole,
    child: Child,
    pub rx: mpsc::Receiver<SessionMessage>,
}

#[derive(Debug, Clone)]
pub enum SessionRole {
    Implementation,
    Stakeholder { name: String },
    LeaderDecision,
}

impl HeadlessSession {
    /// Spawn a new `claude -p` session.
    pub fn spawn(
        step_id: &str,
        role: SessionRole,
        prompt: &str,
        working_dir: &Path,
        claude_session_id: Option<&str>,
    ) -> io::Result<Self> {
        let id = SessionId(format!("{}-{}", step_id, match &role {
            SessionRole::Implementation => "impl".to_string(),
            SessionRole::Stakeholder { name } => name.clone(),
            SessionRole::LeaderDecision => "leader".to_string(),
        }));

        let mut cmd = Command::new("claude");
        cmd.arg("-p")
           .arg(prompt)
           .arg("--output-format").arg("stream-json")
           .current_dir(working_dir)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        if let Some(sid) = claude_session_id {
            cmd.arg("--resume").arg(sid);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout"))?;

        let (tx, rx) = mpsc::channel();

        // Reader thread: parse stream-json lines, forward as events
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.is_empty() => continue,
                    Ok(line) => {
                        let event = parse_stream_line(&line)
                            .unwrap_or(StreamEvent::Other(line.clone()));
                        let _ = tx.send(SessionMessage::Event(event));
                        let _ = tx.send(SessionMessage::RawLine(line));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self { id, step_id: step_id.to_string(), role, child, rx })
    }

    /// Check if the process has exited. Returns exit code if done.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(-1))),
            None => Ok(None),
        }
    }

    /// Kill the process.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}
```

- [ ] **Step 2: Add `pub mod session;` to lib.rs**

- [ ] **Step 3: Verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles without errors.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add headless session runner"
```

---

### Task 6: Orchestrator Lifecycle State Machine

**Files:**
- Create: `src/lifecycle.rs`
- Create: `tests/lifecycle_test.rs`

Pure state machine following the kbtz-tmux pattern: a `tick` function takes a world snapshot, returns actions. No I/O.

- [ ] **Step 1: Write tests for lifecycle transitions**

Create `tests/lifecycle_test.rs`:

```rust
use kbtz_orchestrator::lifecycle::{WorldSnapshot, StepSnapshot, SessionSnapshot, Action, tick};
use kbtz_orchestrator::step::StepPhase;
use kbtz_orchestrator::session::SessionRole;

fn empty_world() -> WorldSnapshot {
    WorldSnapshot { steps: vec![], sessions: vec![], leader_busy: false }
}

#[test]
fn dispatched_step_with_no_session_spawns_implementation() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Dispatched,
            repos: vec!["backend".into()],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::SpawnImplementation { step_id, repos } => {
            assert_eq!(step_id, "step-001");
            assert_eq!(repos, &vec!["backend".to_string()]);
        }
        other => panic!("expected SpawnImplementation, got {:?}", other),
    }
}

#[test]
fn running_step_with_exited_session_transitions_to_completed() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Running,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            step_id: "step-001".into(),
            role: SessionRole::Implementation,
            exited: true,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(a, Action::TransitionStep { step_id, to } if step_id == "step-001" && matches!(to, StepPhase::Completed))));
}

#[test]
fn completed_step_spawns_stakeholders() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Completed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(a, Action::SpawnStakeholders { step_id } if step_id == "step-001")));
}

#[test]
fn reviewing_step_all_stakeholders_exited_transitions_to_reviewed() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewing,
            repos: vec![],
        }],
        sessions: vec![
            SessionSnapshot { step_id: "step-001".into(), role: SessionRole::Stakeholder { name: "security".into() }, exited: true },
            SessionSnapshot { step_id: "step-001".into(), role: SessionRole::Stakeholder { name: "api".into() }, exited: true },
        ],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(a, Action::TransitionStep { step_id, to } if step_id == "step-001" && matches!(to, StepPhase::Reviewed))));
}

#[test]
fn reviewed_step_invokes_leader_when_not_busy() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(a, Action::InvokeLeader { step_ids } if step_ids.contains(&"step-001".to_string()))));
}

#[test]
fn reviewed_step_waits_when_leader_busy() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: true,
    };

    let actions = tick(&world);
    assert!(actions.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: FAIL.

- [ ] **Step 3: Implement lifecycle state machine**

Create `src/lifecycle.rs`:

```rust
use crate::session::SessionRole;
use crate::step::StepPhase;

#[derive(Debug, Clone)]
pub struct StepSnapshot {
    pub id: String,
    pub phase: StepPhase,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub step_id: String,
    pub role: SessionRole,
    pub exited: bool,
}

#[derive(Debug)]
pub struct WorldSnapshot {
    pub steps: Vec<StepSnapshot>,
    pub sessions: Vec<SessionSnapshot>,
    pub leader_busy: bool,
}

#[derive(Debug)]
pub enum Action {
    SpawnImplementation { step_id: String, repos: Vec<String> },
    SpawnStakeholders { step_id: String },
    InvokeLeader { step_ids: Vec<String> },
    TransitionStep { step_id: String, to: StepPhase },
}

pub fn tick(world: &WorldSnapshot) -> Vec<Action> {
    let mut actions = Vec::new();

    for step in &world.steps {
        match &step.phase {
            StepPhase::Dispatched => {
                // No implementation session yet — spawn one
                let has_impl = world.sessions.iter().any(|s| {
                    s.step_id == step.id && matches!(s.role, SessionRole::Implementation)
                });
                if !has_impl {
                    actions.push(Action::SpawnImplementation {
                        step_id: step.id.clone(),
                        repos: step.repos.clone(),
                    });
                }
            }
            StepPhase::Running => {
                // Check if implementation session exited
                let impl_exited = world.sessions.iter().any(|s| {
                    s.step_id == step.id
                        && matches!(s.role, SessionRole::Implementation)
                        && s.exited
                });
                if impl_exited {
                    actions.push(Action::TransitionStep {
                        step_id: step.id.clone(),
                        to: StepPhase::Completed,
                    });
                }
            }
            StepPhase::Completed => {
                // Spawn stakeholder reviews
                let has_stakeholders = world.sessions.iter().any(|s| {
                    s.step_id == step.id && matches!(s.role, SessionRole::Stakeholder { .. })
                });
                if !has_stakeholders {
                    actions.push(Action::SpawnStakeholders {
                        step_id: step.id.clone(),
                    });
                }
            }
            StepPhase::Reviewing => {
                // Check if all stakeholder sessions exited
                let stakeholder_sessions: Vec<_> = world.sessions.iter()
                    .filter(|s| s.step_id == step.id && matches!(s.role, SessionRole::Stakeholder { .. }))
                    .collect();
                if !stakeholder_sessions.is_empty() && stakeholder_sessions.iter().all(|s| s.exited) {
                    actions.push(Action::TransitionStep {
                        step_id: step.id.clone(),
                        to: StepPhase::Reviewed,
                    });
                }
            }
            StepPhase::Reviewed => {
                // Invoke leader with all reviewed steps (batch)
                if !world.leader_busy {
                    // Collect all reviewed steps into one leader invocation
                    // (handled below after the loop to deduplicate)
                }
            }
            StepPhase::Merged | StepPhase::Rework => {
                // Terminal states handled by orchestrator main loop
            }
        }
    }

    // Batch all reviewed steps into one leader invocation
    if !world.leader_busy {
        let reviewed_ids: Vec<String> = world.steps.iter()
            .filter(|s| matches!(s.phase, StepPhase::Reviewed))
            .map(|s| s.id.clone())
            .collect();
        if !reviewed_ids.is_empty() {
            actions.push(Action::InvokeLeader { step_ids: reviewed_ids });
        }
    }

    actions
}
```

- [ ] **Step 4: Add `pub mod lifecycle;` to lib.rs, add `Clone` derive to `SessionRole` if missing, run tests**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo test -p kbtz-orchestrator`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add pure lifecycle state machine"
```

---

### Task 7: MCP Server — Leader Tools

**Files:**
- Create: `src/mcp.rs`

The MCP server runs as a stdio transport subprocess that the leader's Claude Code session connects to. It receives JSON-RPC requests for `define_project`, `dispatch_step`, `rework_step`, `close_step` and translates them into actions the orchestrator processes.

MCP stdio transport: the server reads JSON-RPC from stdin and writes responses to stdout. The orchestrator spawns this as a child process and communicates with it via pipes. The MCP server writes tool call requests to a channel that the orchestrator reads.

- [ ] **Step 1: Implement MCP message types**

Create `src/mcp.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc;

/// An MCP tool call from the leader, to be processed by the orchestrator.
#[derive(Debug, Clone)]
pub enum LeaderRequest {
    DefineProject {
        id: Value,
        repos: Vec<RepoParam>,
        stakeholders: Vec<StakeholderParam>,
        goal_summary: String,
    },
    DispatchStep {
        id: Value,
        prompt: String,
        repos: Vec<String>,
        files: Vec<String>,
    },
    ReworkStep {
        id: Value,
        step_id: String,
        feedback: String,
    },
    CloseStep {
        id: Value,
        step_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoParam {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StakeholderParam {
    pub name: String,
    pub persona: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    result: Value,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    params: Option<Value>,
}

/// MCP tool definitions for the `tools/list` response.
fn tool_definitions() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "define_project",
                "description": "Define the project: register repos, stakeholders, and goal.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "url": { "type": "string" }
                                },
                                "required": ["name", "url"]
                            }
                        },
                        "stakeholders": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "persona": { "type": "string" }
                                },
                                "required": ["name", "persona"]
                            }
                        },
                        "goal_summary": { "type": "string" }
                    },
                    "required": ["repos", "stakeholders", "goal_summary"]
                }
            },
            {
                "name": "dispatch_step",
                "description": "Dispatch an implementation step. Returns the assigned step ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "repos": { "type": "array", "items": { "type": "string" } },
                        "files": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["prompt", "repos"]
                }
            },
            {
                "name": "rework_step",
                "description": "Send a step back to the implementation agent with feedback.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "step_id": { "type": "string" },
                        "feedback": { "type": "string" }
                    },
                    "required": ["step_id", "feedback"]
                }
            },
            {
                "name": "close_step",
                "description": "Close a step (merged or abandoned). Cleans up session directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "step_id": { "type": "string" }
                    },
                    "required": ["step_id"]
                }
            }
        ]
    })
}

/// Run the MCP stdio server. Reads JSON-RPC from stdin, sends LeaderRequests
/// to the provided channel, and writes responses to stdout.
///
/// This function blocks and should be run in its own thread or process.
pub fn run_mcp_server(tx: mpsc::Sender<LeaderRequest>, rx_response: mpsc::Receiver<Value>) -> io::Result<()> {
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match request.method.as_str() {
            "initialize" => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id,
                    result: serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "kbtz-orchestrator", "version": "0.1.0" }
                    }),
                };
                writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                writer.flush()?;
            }
            "tools/list" => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id,
                    result: tool_definitions(),
                };
                writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                writer.flush()?;
            }
            "tools/call" => {
                let params = request.params.unwrap_or(Value::Null);
                let tool_name = params["name"].as_str().unwrap_or("");
                let arguments = &params["arguments"];

                let leader_request = match tool_name {
                    "define_project" => {
                        let repos: Vec<RepoParam> = serde_json::from_value(arguments["repos"].clone()).unwrap_or_default();
                        let stakeholders: Vec<StakeholderParam> = serde_json::from_value(arguments["stakeholders"].clone()).unwrap_or_default();
                        let goal_summary = arguments["goal_summary"].as_str().unwrap_or("").to_string();
                        LeaderRequest::DefineProject { id: request.id.clone(), repos, stakeholders, goal_summary }
                    }
                    "dispatch_step" => {
                        let prompt = arguments["prompt"].as_str().unwrap_or("").to_string();
                        let repos: Vec<String> = serde_json::from_value(arguments["repos"].clone()).unwrap_or_default();
                        let files: Vec<String> = serde_json::from_value(arguments["files"].clone()).unwrap_or_default();
                        LeaderRequest::DispatchStep { id: request.id.clone(), prompt, repos, files }
                    }
                    "rework_step" => {
                        let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();
                        let feedback = arguments["feedback"].as_str().unwrap_or("").to_string();
                        LeaderRequest::ReworkStep { id: request.id.clone(), step_id, feedback }
                    }
                    "close_step" => {
                        let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();
                        LeaderRequest::CloseStep { id: request.id.clone(), step_id }
                    }
                    _ => continue,
                };

                // Send to orchestrator and wait for response
                let _ = tx.send(leader_request);
                if let Ok(result) = rx_response.recv() {
                    let response = JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: request.id,
                        result,
                    };
                    writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                    writer.flush()?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Add `pub mod mcp;` to lib.rs, verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add MCP server for leader tools"
```

---

### Task 8: Leader and Agent Prompts

**Files:**
- Create: `src/prompt.rs`

- [ ] **Step 1: Implement prompt templates**

Create `src/prompt.rs`:

```rust
use crate::project::OrchestratorState;
use crate::step::StepPhase;

/// Build the system prompt for the leader session.
/// This is appended via --append-system-prompt on interactive sessions
/// or included in the -p prompt for headless invocations.
pub fn leader_system_prompt() -> String {
    r#"You are the leader of an AI agent orchestration project. You have MCP tools
to manage the project:

- define_project(repos, stakeholders, goal_summary): Register the repos and
  stakeholder reviewers for this project. Call this first.
- dispatch_step(prompt, repos, files): Dispatch an implementation step.
  Describe what the implementation agent should do in the prompt. Specify
  which repos are relevant. Returns a step ID.
- rework_step(step_id, feedback): Send a completed step back to the
  implementation agent with feedback for changes.
- close_step(step_id): Close a step after you've merged its changes (or
  decided to abandon it). This cleans up the session directory.

After defining the project, save the project definition to project.md in
your working directory. Read this file at the start of every session to
recall the project state.

When invoked with feedback, review all stakeholder feedback, form your own
judgment, then either merge the implementation branch and call close_step,
or call rework_step with specific feedback. You can also dispatch new
follow-up steps.

Merge implementation branches using git merge or git cherry-pick in the
repos under repos/. Resolve any conflicts."#
        .to_string()
}

/// Build the headless leader prompt with full state snapshot and feedback.
pub fn leader_decision_prompt(state: &OrchestratorState, step_feedback: &[(String, Vec<(String, String)>)]) -> String {
    let mut prompt = String::new();

    prompt.push_str("# Current Project State\n\n");
    prompt.push_str(&format!("**Goal:** {}\n\n", state.project.goal_summary));

    prompt.push_str("**Repos:**\n");
    for repo in &state.project.repos {
        prompt.push_str(&format!("- {} ({})\n", repo.name, repo.url));
    }

    prompt.push_str("\n**All Steps:**\n");
    for step in &state.steps {
        let phase_str = match &step.phase {
            StepPhase::Dispatched => "dispatched",
            StepPhase::Running => "running",
            StepPhase::Completed => "completed",
            StepPhase::Reviewing => "reviewing",
            StepPhase::Reviewed => "REVIEWED — needs your action",
            StepPhase::Merged => "merged",
            StepPhase::Rework => "rework in progress",
        };
        prompt.push_str(&format!("- {} [{}]: {}\n", step.id, phase_str, step.dispatch.prompt));
    }

    if !step_feedback.is_empty() {
        prompt.push_str("\n# Steps Ready for Your Review\n\n");
        for (step_id, feedbacks) in step_feedback {
            prompt.push_str(&format!("## {}\n\n", step_id));
            prompt.push_str(&format!("Branch `{}` has been fetched into your repos.\n\n", step_id));
            for (stakeholder, feedback) in feedbacks {
                prompt.push_str(&format!("### {} feedback\n{}\n\n", stakeholder, feedback));
            }
        }
        prompt.push_str("Review the feedback above. For each step, either:\n");
        prompt.push_str("1. Merge the branch in your repos and call close_step(step_id)\n");
        prompt.push_str("2. Call rework_step(step_id, feedback) with specific changes needed\n\n");
        prompt.push_str("You may also dispatch new follow-up steps if needed.\n");
    }

    prompt
}

/// Build the prompt for an implementation agent session.
pub fn implementation_prompt(step_prompt: &str) -> String {
    format!(
        r#"You are an implementation agent. Your task:

{}

Your working directory contains the repo(s) you need to modify. If there are
multiple repos, they are in subdirectories. A `files/` directory may contain
additional context from the project leader.

Do the work, commit your changes, and provide a summary of what you did.
Make clean, focused commits. Do not push to any remote."#,
        step_prompt
    )
}

/// Build the prompt for a stakeholder review session.
pub fn stakeholder_prompt(persona: &str, step_prompt: &str, summary: &str) -> String {
    format!(
        r#"You are a code reviewer with the following focus:

{}

## Step being reviewed

**Task:** {}

**Implementation summary:** {}

Review the implementation. The repo clones are available in the working
directory for you to read code and commit history. The leader's full repos
are also available for broader context.

Provide structured feedback: what's good, what needs changes, and any
blocking concerns. Be specific — reference file paths and line numbers."#,
        persona, step_prompt, summary
    )
}
```

- [ ] **Step 2: Add `pub mod prompt;` to lib.rs, verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add prompt templates for leader, implementors, stakeholders"
```

---

### Task 9: TUI — Dashboard Panel

**Files:**
- Create: `src/tui/mod.rs`
- Create: `src/tui/dashboard.rs`
- Create: `src/tui/stream_view.rs`

- [ ] **Step 1: Implement TUI app state**

Create `src/tui/mod.rs`:

```rust
pub mod dashboard;
pub mod stream_view;

use crate::project::OrchestratorState;
use crate::stream::StreamEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Dashboard,
    Leader,
}

pub struct AppState {
    pub view: View,
    pub selected_session: Option<String>,
    pub session_events: Vec<(String, Vec<StreamEvent>)>,
    pub leader_idle: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            view: View::Dashboard,
            selected_session: None,
            session_events: vec![],
            leader_idle: true,
        }
    }

    pub fn push_event(&mut self, session_id: &str, event: StreamEvent) {
        if let Some((_, events)) = self.session_events.iter_mut().find(|(id, _)| id == session_id) {
            events.push(event);
        } else {
            self.session_events.push((session_id.to_string(), vec![event]));
        }
    }

    pub fn selected_events(&self) -> &[StreamEvent] {
        if let Some(ref sid) = self.selected_session {
            if let Some((_, events)) = self.session_events.iter().find(|(id, _)| id == sid) {
                return events;
            }
        }
        &[]
    }
}
```

- [ ] **Step 2: Implement dashboard rendering**

Create `src/tui/dashboard.rs`:

```rust
use crate::step::{Step, StepPhase};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

pub fn render_dashboard(frame: &mut Frame, area: Rect, steps: &[Step], sessions: &[String], selected_session: &Option<String>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(5),
        ])
        .split(area);

    // Header
    let header = Paragraph::new("kbtz-orchestrator")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    // Step list
    let step_items: Vec<ListItem> = steps.iter().map(|step| {
        let phase = match &step.phase {
            StepPhase::Dispatched => ("DISPATCHED", Color::Yellow),
            StepPhase::Running => ("RUNNING", Color::Blue),
            StepPhase::Completed => ("COMPLETED", Color::Green),
            StepPhase::Reviewing => ("REVIEWING", Color::Magenta),
            StepPhase::Reviewed => ("REVIEWED", Color::Cyan),
            StepPhase::Merged => ("MERGED", Color::DarkGray),
            StepPhase::Rework => ("REWORK", Color::Red),
        };
        let line = Line::from(vec![
            Span::styled(format!("{} ", step.id), Style::default().fg(Color::White)),
            Span::styled(format!("[{}] ", phase.0), Style::default().fg(phase.1)),
            Span::raw(&step.dispatch.prompt),
        ]);
        ListItem::new(line)
    }).collect();

    let step_list = List::new(step_items)
        .block(Block::default().title(" Steps ").borders(Borders::ALL));
    frame.render_widget(step_list, chunks[1]);

    // Session list
    let session_items: Vec<ListItem> = sessions.iter().map(|s| {
        let style = if Some(s) == selected_session.as_ref() {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        ListItem::new(Span::styled(s.clone(), style))
    }).collect();

    let session_list = List::new(session_items)
        .block(Block::default().title(" Sessions ").borders(Borders::ALL));
    frame.render_widget(session_list, chunks[2]);
}
```

- [ ] **Step 3: Implement stream-json viewer**

Create `src/tui/stream_view.rs`:

```rust
use crate::stream::StreamEvent;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_stream_view(frame: &mut Frame, area: Rect, events: &[StreamEvent], session_id: &str) {
    let lines: Vec<Line> = events.iter().flat_map(|event| {
        match event {
            StreamEvent::Thinking(text) => vec![
                Line::from(Span::styled(
                    format!("[thinking] {}", truncate(text, 200)),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )),
            ],
            StreamEvent::AssistantText(text) => vec![
                Line::from(Span::styled(text.clone(), Style::default().fg(Color::White))),
            ],
            StreamEvent::ToolUse { name, input } => vec![
                Line::from(Span::styled(
                    format!("[tool] {} {}", name, truncate(input, 100)),
                    Style::default().fg(Color::Yellow),
                )),
            ],
            StreamEvent::ToolResult { content } => vec![
                Line::from(Span::styled(
                    format!("[result] {}", truncate(content, 100)),
                    Style::default().fg(Color::Green),
                )),
            ],
            StreamEvent::Result { result } => vec![
                Line::from(Span::styled(
                    format!("[done] {}", truncate(result, 200)),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
            ],
            StreamEvent::Other(_) => vec![],
        }
    }).collect();

    let title = format!(" {} ", session_id);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max])
    } else {
        s.to_string()
    }
}
```

- [ ] **Step 4: Add `pub mod tui;` to lib.rs, update Cargo.toml with dependencies**

Add to `kbtz-orchestrator/Cargo.toml`:

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
ratatui = { version = "0.30", features = ["crossterm_0_28"] }
crossterm = "0.28"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 5: Verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add TUI dashboard and stream-json viewer"
```

---

### Task 10: TUI — Interactive Leader PTY Embedding

**Files:**
- Create: `src/tui/leader.rs`
- Modify: `src/tui/mod.rs`

This task reuses kbtz-workspace's PTY spawning and raw byte forwarding
approach. The orchestrator embeds an interactive Claude Code session for
the leader.

- [ ] **Step 1: Add kbtz-workspace dependency**

Add to `kbtz-orchestrator/Cargo.toml`:

```toml
portable-pty = "0.9"
vt100 = "0.16"
libc = "0.2"
```

And the local workspace dep:

```toml
kbtz-workspace = { path = "../kbtz-workspace" }
```

- [ ] **Step 2: Implement leader PTY session**

Create `src/tui/leader.rs`:

```rust
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

pub enum LeaderMessage {
    Output(Vec<u8>),
    Exited,
}

pub struct LeaderSession {
    writer: Box<dyn Write + Send>,
    pub rx: mpsc::Receiver<LeaderMessage>,
    child: Box<dyn portable_pty::Child + Send>,
}

impl LeaderSession {
    /// Spawn an interactive leader Claude Code session.
    pub fn spawn(
        working_dir: &std::path::Path,
        session_id: Option<&str>,
        mcp_config_path: &std::path::Path,
        system_prompt: &str,
        rows: u16,
        cols: u16,
    ) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut cmd = CommandBuilder::new("claude");
        cmd.arg("--append-system-prompt").arg(system_prompt);
        if let Some(sid) = session_id {
            cmd.arg("--resume").arg(sid);
        }
        // Point Claude Code at the MCP server config
        cmd.arg("--mcp-config").arg(mcp_config_path);
        cmd.cwd(working_dir);

        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut reader = pair.master.try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let writer = pair.master.take_writer()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let (tx, rx) = mpsc::channel();

        // Reader thread
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(LeaderMessage::Exited);
                        break;
                    }
                    Ok(n) => {
                        let _ = tx.send(LeaderMessage::Output(buf[..n].to_vec()));
                    }
                    Err(_) => {
                        let _ = tx.send(LeaderMessage::Exited);
                        break;
                    }
                }
            }
        });

        Ok(Self { writer, rx, child })
    }

    pub fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    pub fn resize(&self, _rows: u16, _cols: u16) -> io::Result<()> {
        // PTY resize would go here — portable-pty handles this via the master
        Ok(())
    }

    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}
```

- [ ] **Step 3: Add leader module to tui/mod.rs**

Add `pub mod leader;` to `src/tui/mod.rs`.

- [ ] **Step 4: Verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add interactive leader PTY session"
```

---

### Task 11: CLI Entry Point and Main Event Loop

**Files:**
- Modify: `src/main.rs`

This wires everything together: CLI parsing, project init/load, TUI rendering, event loop that polls sessions and dispatches lifecycle actions.

- [ ] **Step 1: Implement CLI and main loop**

Replace `src/main.rs`:

```rust
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::{Constraint, Direction, Layout}, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use kbtz_orchestrator::project::ProjectDir;
use kbtz_orchestrator::tui::{AppState, View};
use kbtz_orchestrator::tui::dashboard::render_dashboard;
use kbtz_orchestrator::tui::stream_view::render_stream_view;

#[derive(Parser)]
#[command(name = "kbtz-orchestrator")]
#[command(about = "Leader-driven AI agent orchestrator")]
struct Cli {
    /// Path to the project directory. Created if it doesn't exist.
    #[arg(short, long)]
    project: PathBuf,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    // Init terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = AppState::new();

    // Main event loop
    let result = run_loop(&mut terminal, &mut app, &cli.project);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    project_path: &PathBuf,
) -> io::Result<()> {
    loop {
        // Render
        terminal.draw(|frame| {
            let area = frame.area();

            match app.view {
                View::Dashboard => {
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(40),
                            Constraint::Percentage(60),
                        ])
                        .split(area);

                    // Load state for rendering (in real impl, cache this)
                    let steps = if let Ok(dir) = ProjectDir::load(project_path) {
                        dir.state().steps.clone()
                    } else {
                        vec![]
                    };

                    let sessions: Vec<String> = app.session_events.iter()
                        .map(|(id, _)| id.clone())
                        .collect();

                    render_dashboard(frame, chunks[0], &steps, &sessions, &app.selected_session);

                    let events = app.selected_events();
                    let session_id = app.selected_session.as_deref().unwrap_or("(none)");
                    render_stream_view(frame, chunks[1], events, session_id);
                }
                View::Leader => {
                    // Leader PTY rendering handled separately via raw byte forwarding
                }
            }
        })?;

        // Handle input
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Tab => {
                        // Cycle selected session
                        let sessions: Vec<String> = app.session_events.iter()
                            .map(|(id, _)| id.clone())
                            .collect();
                        if !sessions.is_empty() {
                            let current_idx = app.selected_session.as_ref()
                                .and_then(|s| sessions.iter().position(|id| id == s))
                                .map(|i| (i + 1) % sessions.len())
                                .unwrap_or(0);
                            app.selected_session = Some(sessions[current_idx].clone());
                        }
                    }
                    KeyCode::Char('l') if app.leader_idle => {
                        app.view = View::Leader;
                        // Leader session spawn would go here
                    }
                    KeyCode::Esc => {
                        app.view = View::Dashboard;
                    }
                    _ => {}
                }
            }
        }

        // Poll headless sessions for new events
        // (In full implementation: iterate sessions, try_recv from each, push events)
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add CLI entry point and main event loop"
```

---

### Task 12: Integration — Wire Lifecycle to Session Spawning

**Files:**
- Modify: `src/main.rs`

This task connects the lifecycle tick function to actual session spawning
and state transitions. It's the core orchestration wiring that makes the
system work end-to-end.

This is a large integration task. The implementation agent should:

- [ ] **Step 1: Add an `Orchestrator` struct to main.rs (or a new `orchestrator.rs`)**

The struct holds:
- `ProjectDir` — the project state
- `Vec<HeadlessSession>` — active headless sessions
- `Option<LeaderSession>` — the interactive leader, if attached
- `AppState` — TUI state
- MCP channel endpoints

- [ ] **Step 2: Implement a `process_tick` method**

This method:
1. Builds a `WorldSnapshot` from current state
2. Calls `lifecycle::tick()`
3. Processes each returned `Action`:
   - `SpawnImplementation`: call `git::setup_session_dir`, then `HeadlessSession::spawn` with `implementation_prompt`
   - `SpawnStakeholders`: for each stakeholder in project config, spawn a `HeadlessSession` with `stakeholder_prompt`
   - `InvokeLeader`: build `leader_decision_prompt` with full state + feedback, spawn headless leader session
   - `TransitionStep`: update step phase in `ProjectDir`, persist

- [ ] **Step 3: Implement session polling in the main loop**

In the main event loop, after handling keyboard input:
1. For each active `HeadlessSession`, call `try_recv()` on its channel
2. Forward `StreamEvent`s to `AppState` for TUI display
3. Call `try_wait()` to detect exits
4. On exit: extract results (summary from stream events, commits via `git::fetch_branch`)
5. Call `process_tick()` to advance the state machine

- [ ] **Step 4: Implement MCP request handling**

In the main loop, check the MCP channel for `LeaderRequest`s:
- `DefineProject`: init `ProjectDir`, clone repos
- `DispatchStep`: call `project_dir.add_step()`, let tick handle spawning
- `ReworkStep`: transition step to `Rework`, spawn resumed impl session
- `CloseStep`: transition step to `Merged`, call `git::cleanup_session_dir`
Send response back through MCP response channel.

- [ ] **Step 5: Test end-to-end manually**

```bash
cd /home/virgil/kbtz/worktrees/orchestrator-design
cargo build -p kbtz-orchestrator
# Create a test project directory
mkdir -p /tmp/test-project
# Run the orchestrator
./target/debug/kbtz-orchestrator --project /tmp/test-project
```

Verify: TUI renders, 'q' quits cleanly, 'l' would open leader view.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): wire lifecycle state machine to session spawning"
```

---

### Task 13: MCP Config File Generation

**Files:**
- Modify: `src/mcp.rs` or create `src/mcp_config.rs`

The leader's Claude Code session needs an `.mcp.json` config file that
points at the orchestrator's MCP server. The orchestrator generates this
file in the project directory at startup.

- [ ] **Step 1: Implement MCP config generation**

Add a function that writes the MCP config file:

```rust
use std::path::Path;
use std::io;

/// Write an .mcp.json config file for the leader's Claude Code session.
/// The MCP server is the orchestrator itself, run via stdio transport.
pub fn write_mcp_config(project_dir: &Path, orchestrator_binary: &str) -> io::Result<std::path::PathBuf> {
    let config = serde_json::json!({
        "mcpServers": {
            "orchestrator": {
                "command": orchestrator_binary,
                "args": ["--mcp-mode", "--project", project_dir.to_str().unwrap()],
                "type": "stdio"
            }
        }
    });

    let config_path = project_dir.join(".mcp.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())?;
    Ok(config_path)
}
```

- [ ] **Step 2: Add `--mcp-mode` flag to CLI**

When the orchestrator is invoked with `--mcp-mode`, it runs the MCP stdio
server instead of the TUI. This lets the Claude Code leader session talk
to the orchestrator via the MCP config above.

Add to `Cli`:

```rust
/// Run as MCP stdio server (used by Claude Code, not for direct invocation)
#[arg(long)]
mcp_mode: bool,
```

In `main()`, branch on this flag:

```rust
if cli.mcp_mode {
    let (tx, rx) = std::sync::mpsc::channel();
    let (resp_tx, resp_rx) = std::sync::mpsc::channel();
    // Run MCP server on stdio
    kbtz_orchestrator::mcp::run_mcp_server(tx, resp_rx)?;
    return Ok(());
}
```

- [ ] **Step 3: Verify compilation**

Run: `cd /home/virgil/kbtz/worktrees/orchestrator-design && cargo check -p kbtz-orchestrator`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(orchestrator): add MCP config generation and --mcp-mode flag"
```
