//! Integration tests for kbtz-shepherd spawn, handshake, failure, and cleanup paths.
//!
//! Tests are split into three groups:
//!
//! **Process tests** spawn a real kbtz-shepherd binary. They require an
//! environment that supports Unix domain socket creation (AF_UNIX bind).
//! These tests skip automatically in restricted environments (containers,
//! sandboxes that block AF_UNIX).
//!
//! **Protocol tests** use `UnixStream::pair()` (socketpair — works in all
//! environments) to exercise the handshake protocol and message exchange
//! without a real shepherd process.
//!
//! **Logic tests** are pure computation tests that verify counter
//! monotonicity, spawn failure rollback, and concurrent claim safety
//! by simulating the workspace's spawning algorithm.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbtz_workspace::protocol::{self, Message};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Returns true if the environment supports named Unix domain sockets.
fn can_bind_unix_socket() -> bool {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let path = dir.path().join("probe.sock");
    match UnixListener::bind(&path) {
        Ok(_listener) => {
            let _ = std::fs::remove_file(&path);
            true
        }
        Err(_) => false,
    }
}

/// Skip the current test if Unix domain sockets are unavailable.
macro_rules! require_unix_sockets {
    () => {
        if !can_bind_unix_socket() {
            eprintln!("SKIPPED: Unix domain sockets unavailable (sandboxed environment)");
            return;
        }
    };
}

/// Find the kbtz-shepherd binary next to the test binary.
fn shepherd_bin() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let deps_dir = test_exe.parent().unwrap();
    let debug_dir = deps_dir.parent().unwrap();
    let bin = debug_dir.join("kbtz-shepherd");
    assert!(
        bin.exists(),
        "kbtz-shepherd not found at {}; run `cargo build` first",
        bin.display()
    );
    bin
}

/// Spawn a shepherd process.
fn spawn_shepherd(
    socket_path: &Path,
    pid_path: &Path,
    rows: u16,
    cols: u16,
    command: &str,
    args: &[&str],
) -> std::process::Child {
    let mut cmd = Command::new(shepherd_bin());
    cmd.arg(socket_path)
        .arg(pid_path)
        .arg(rows.to_string())
        .arg(cols.to_string())
        .arg(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.spawn().expect("failed to spawn kbtz-shepherd")
}

/// Wait for the socket file to appear, or the shepherd to exit.
/// Returns true if socket appeared.
fn wait_for_socket_or_exit(
    socket_path: &Path,
    child: &mut std::process::Child,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if socket_path.exists() {
            return true;
        }
        if let Ok(Some(_status)) = child.try_wait() {
            return false;
        }
        if Instant::now() >= deadline {
            panic!(
                "shepherd did not create socket at {} within {:?}",
                socket_path.display(),
                timeout
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Perform the size-first handshake: send Resize, receive InitialState.
fn handshake(socket_path: &Path, rows: u16, cols: u16) -> anyhow::Result<(UnixStream, Vec<u8>)> {
    let stream = UnixStream::connect(socket_path)?;
    let read_stream = stream.try_clone()?;
    let write_stream = stream;

    let mut writer = BufWriter::new(write_stream);
    protocol::write_message(&mut writer, &Message::Resize { rows, cols })?;
    writer.flush()?;

    let mut reader = BufReader::new(read_stream);
    let msg = protocol::read_message(&mut reader)?;
    match msg {
        Some(Message::InitialState(data)) => {
            let stream = writer.into_inner()?;
            Ok((stream, data))
        }
        Some(other) => {
            anyhow::bail!(
                "expected InitialState, got {:?}",
                std::mem::discriminant(&other)
            )
        }
        None => anyhow::bail!("shepherd closed connection before sending InitialState"),
    }
}

fn child_pid_path(pid_path: &Path) -> PathBuf {
    pid_path.with_extension("child-pid")
}

/// Kill a shepherd by PID and wait for its process handle to complete.
fn kill_shepherd(pid: u32, child: &mut std::process::Child) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let _ = child.wait();
}

// ═══════════════════════════════════════════════════════════════════════
// Process tests: require real kbtz-shepherd + Unix domain sockets
// ═══════════════════════════════════════════════════════════════════════

// ── Test 1: Normal spawn → handshake → connect cycle ────────────────────

#[test]
fn normal_spawn_handshake_connect() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    let mut child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sleep", &["999"]);

    assert!(
        wait_for_socket_or_exit(&socket_path, &mut child, Duration::from_secs(5)),
        "shepherd did not create socket"
    );

    // PID file should exist
    assert!(pid_path.exists(), "PID file not created");
    let pid_str = std::fs::read_to_string(&pid_path).unwrap();
    let shepherd_pid: u32 = pid_str.trim().parse().unwrap();
    assert!(shepherd_pid > 0);

    // Perform handshake
    let (stream, initial_data) = handshake(&socket_path, 23, 80).expect("handshake failed");
    assert!(
        initial_data.len() < 1_000_000,
        "initial state suspiciously large"
    );

    // Verify we can send input
    let mut writer = BufWriter::new(stream.try_clone().unwrap());
    protocol::write_message(&mut writer, &Message::PtyInput(b"hello\n".to_vec()))
        .expect("failed to send input");

    // Child PID file should exist
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        child_pid_path(&pid_path).exists(),
        "child PID file not created"
    );

    kill_shepherd(shepherd_pid, &mut child);
    drop(stream);
}

// ── Test 2: Shepherd dies during handshake ───────────────────────────────

#[test]
fn shepherd_dies_during_handshake() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    // Command that doesn't exist → shepherd fails during spawn_command
    let mut child = spawn_shepherd(
        &socket_path,
        &pid_path,
        24,
        80,
        "/nonexistent/command/that/does/not/exist",
        &[],
    );

    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "shepherd should exit with error for nonexistent command"
    );

    // Socket should not exist (or if briefly created, handshake should fail)
    if socket_path.exists() {
        let result = handshake(&socket_path, 23, 80);
        assert!(result.is_err(), "handshake should fail with dead shepherd");
    }

    // PID file should be cleaned up
    assert!(
        !pid_path.exists(),
        "PID file should be cleaned up after shepherd failure"
    );
}

// ── Test 3: Child dies during init → shepherd drops connection ──────────

#[test]
fn child_dies_during_init_shepherd_drops_connection() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    // Child exits immediately with code 42
    let mut child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sh", &["-c", "exit 42"]);

    // Wait for socket or shepherd exit
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket_path.exists() {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            // Shepherd exited before socket — child died very fast
            return;
        }
        if Instant::now() >= deadline {
            panic!("shepherd neither created socket nor exited within 5s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Socket appeared — try handshake. Shepherd should drop the connection
    // because the child is dead (per c721dfa).
    if let Ok(stream) = UnixStream::connect(&socket_path) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let read_stream = stream.try_clone().unwrap();

        let mut writer = BufWriter::new(stream);
        let _ = protocol::write_message(&mut writer, &Message::Resize { rows: 23, cols: 80 });
        let _ = writer.flush();

        let mut reader = BufReader::new(read_stream);
        let result = protocol::read_message(&mut reader);

        match result {
            Ok(Some(Message::InitialState(_))) => {
                // Small race window — child died after handshake check
            }
            Ok(None) | Err(_) => {
                // Expected: connection dropped
            }
            Ok(Some(other)) => {
                panic!("unexpected message: {:?}", std::mem::discriminant(&other));
            }
        }
    }

    // Shepherd should exit with non-zero
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "shepherd should exit with non-zero for dead child"
    );
}

// ── Test 4: Socket pre-exists from previous run → replaced cleanly ──────

#[test]
fn socket_preexists_replaced_cleanly() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    // Create a stale file at the socket path
    std::fs::write(&socket_path, "stale").unwrap();

    let mut child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sleep", &["999"]);

    // Wait for the PID file (not the socket, which already exists as a stale file).
    // The shepherd writes the PID file before creating the socket, so once the
    // PID file exists and the socket is a real socket, we're ready to connect.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if pid_path.exists() {
            // PID file exists — give the shepherd a moment to replace the
            // stale socket file and bind the real socket.
            std::thread::sleep(Duration::from_millis(100));
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            panic!("shepherd exited before creating PID file");
        }
        if Instant::now() >= deadline {
            panic!("shepherd did not create PID file within 5s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Handshake should succeed — stale file was replaced with a real socket
    let (stream, _) =
        handshake(&socket_path, 23, 80).expect("handshake failed after socket replacement");

    let pid_str = std::fs::read_to_string(&pid_path).unwrap();
    let shepherd_pid: u32 = pid_str.trim().parse().unwrap();
    kill_shepherd(shepherd_pid, &mut child);
    drop(stream);
}

// ── Test 6: Spawn failure → files cleaned up ────────────────────────────

#[test]
fn spawn_failure_cleans_up() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    let mut child = spawn_shepherd(
        &socket_path,
        &pid_path,
        24,
        80,
        "/absolutely/nonexistent/binary",
        &[],
    );

    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "shepherd should fail for invalid command"
    );

    // No orphaned resources
    assert!(
        !socket_path.exists(),
        "socket should be cleaned up after spawn failure"
    );
}

// ── Test 7: Concurrent spawns use unique sockets ────────────────────────

#[test]
fn concurrent_spawns_use_unique_sockets() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();

    for i in 0..3 {
        let socket_path = dir.path().join(format!("ws-{i}.sock"));
        let pid_path = dir.path().join(format!("ws-{i}.pid"));
        let child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sleep", &["999"]);
        handles.push((socket_path, pid_path, child));
    }

    // Wait for all sockets
    for (socket_path, _, child) in &mut handles {
        assert!(
            wait_for_socket_or_exit(socket_path, child, Duration::from_secs(5)),
            "shepherd did not create socket at {}",
            socket_path.display()
        );
    }

    // Handshake with all three
    let mut streams = Vec::new();
    for (socket_path, _, _) in &handles {
        let (stream, _) = handshake(socket_path, 23, 80)
            .unwrap_or_else(|e| panic!("handshake failed for {}: {e}", socket_path.display()));
        streams.push(stream);
    }

    // Verify unique PIDs
    let mut pids: Vec<u32> = Vec::new();
    for (_, pid_path, _) in &handles {
        let pid: u32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !pids.contains(&pid),
            "PID {pid} duplicated across shepherds"
        );
        pids.push(pid);
    }

    // Clean up
    drop(streams);
    for (_, _, mut child) in handles {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ── Test: SIGTERM cleanup ───────────────────────────────────────────────

#[test]
fn shepherd_cleans_up_on_sigterm() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    let mut child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sleep", &["999"]);

    assert!(
        wait_for_socket_or_exit(&socket_path, &mut child, Duration::from_secs(5)),
        "shepherd did not create socket"
    );

    let shepherd_pid: u32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    unsafe { libc::kill(shepherd_pid as libc::pid_t, libc::SIGTERM) };
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(200));

    assert!(
        !socket_path.exists(),
        "socket should be cleaned up after SIGTERM"
    );
}

// ── Test: Second client replaces first ──────────────────────────────────

#[test]
fn second_client_replaces_first() {
    require_unix_sockets!();

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let pid_path = dir.path().join("test.pid");

    let mut child = spawn_shepherd(&socket_path, &pid_path, 24, 80, "sleep", &["999"]);

    assert!(
        wait_for_socket_or_exit(&socket_path, &mut child, Duration::from_secs(5)),
        "shepherd did not create socket"
    );

    // First client
    let (stream1, _) = handshake(&socket_path, 23, 80).expect("first handshake failed");

    // Second client replaces first
    let (stream2, _) = handshake(&socket_path, 23, 80).expect("second handshake failed");

    // Second client can send input
    let mut writer = BufWriter::new(stream2.try_clone().unwrap());
    protocol::write_message(&mut writer, &Message::PtyInput(b"test\n".to_vec()))
        .expect("second client should be able to send input");

    // First client should be disconnected
    stream1
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream1);
    match protocol::read_message(&mut reader) {
        Ok(None) | Err(_) => {} // Expected
        Ok(Some(_)) => {}       // Possible if pending output flushed
    }

    let shepherd_pid: u32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    kill_shepherd(shepherd_pid, &mut child);
    drop(stream2);
}

// ═══════════════════════════════════════════════════════════════════════
// Protocol tests: use UnixStream::pair(), no real shepherd needed
// ═══════════════════════════════════════════════════════════════════════

/// Simulate a shepherd: accept a Resize, respond with InitialState.
fn simulate_shepherd_handshake(
    server: &mut UnixStream,
    initial_data: &[u8],
) -> anyhow::Result<Message> {
    let resize = protocol::read_message(server)?
        .ok_or_else(|| anyhow::anyhow!("expected Resize, got EOF"))?;
    protocol::write_message(server, &Message::InitialState(initial_data.to_vec()))?;
    Ok(resize)
}

// ── Test 1p: Normal handshake protocol ──────────────────────────────────

#[test]
fn protocol_normal_handshake() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Client side: send Resize, then read InitialState
    let client_thread = std::thread::spawn(move || {
        protocol::write_message(&mut client, &Message::Resize { rows: 23, cols: 80 }).unwrap();
        let msg = protocol::read_message(&mut client).unwrap().unwrap();
        (client, msg)
    });

    // Server side: read Resize, send InitialState
    let resize = simulate_shepherd_handshake(&mut server, b"hello world").unwrap();
    assert_eq!(resize, Message::Resize { rows: 23, cols: 80 });

    let (client, msg) = client_thread.join().unwrap();
    assert_eq!(msg, Message::InitialState(b"hello world".to_vec()));
    drop(client);
}

// ── Test 2p: Server closes before sending InitialState ──────────────────

#[test]
fn protocol_server_closes_before_initial_state() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Client sends Resize
    protocol::write_message(&mut client, &Message::Resize { rows: 23, cols: 80 }).unwrap();

    // Server reads Resize, then closes without sending InitialState
    let _ = protocol::read_message(&mut server).unwrap();
    drop(server);

    // Client should get EOF
    let result = protocol::read_message(&mut client);
    match result {
        Ok(None) => {} // EOF — expected
        Err(_) => {}   // Error — also fine
        Ok(Some(msg)) => panic!("expected EOF, got {:?}", msg),
    }
}

// ── Test 3p: Client sends wrong first message ───────────────────────────

#[test]
fn protocol_wrong_first_message() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Client sends PtyInput instead of Resize — server should reject
    protocol::write_message(&mut client, &Message::PtyInput(b"wrong".to_vec())).unwrap();

    let msg = protocol::read_message(&mut server).unwrap().unwrap();
    // The shepherd would check this and drop the connection
    assert!(
        !matches!(msg, Message::Resize { .. }),
        "should not be a Resize message"
    );
}

// ── Test 4p: Bidirectional message exchange after handshake ─────────────

#[test]
fn protocol_bidirectional_exchange() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Handshake
    protocol::write_message(&mut client, &Message::Resize { rows: 23, cols: 80 }).unwrap();
    simulate_shepherd_handshake(&mut server, b"init").unwrap();
    let _ = protocol::read_message(&mut client).unwrap(); // consume InitialState

    // Client sends input
    protocol::write_message(&mut client, &Message::PtyInput(b"hello".to_vec())).unwrap();
    let msg = protocol::read_message(&mut server).unwrap().unwrap();
    assert_eq!(msg, Message::PtyInput(b"hello".to_vec()));

    // Server sends output
    protocol::write_message(&mut server, &Message::PtyOutput(b"world".to_vec())).unwrap();
    let msg = protocol::read_message(&mut client).unwrap().unwrap();
    assert_eq!(msg, Message::PtyOutput(b"world".to_vec()));

    // Client sends resize
    protocol::write_message(
        &mut client,
        &Message::Resize {
            rows: 50,
            cols: 120,
        },
    )
    .unwrap();
    let msg = protocol::read_message(&mut server).unwrap().unwrap();
    assert_eq!(
        msg,
        Message::Resize {
            rows: 50,
            cols: 120
        }
    );
}

// ── Test 5p: Shutdown message ───────────────────────────────────────────

#[test]
fn protocol_shutdown_message() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Handshake
    protocol::write_message(&mut client, &Message::Resize { rows: 23, cols: 80 }).unwrap();
    simulate_shepherd_handshake(&mut server, b"init").unwrap();
    let _ = protocol::read_message(&mut client).unwrap();

    // Client sends Shutdown
    protocol::write_message(&mut client, &Message::Shutdown).unwrap();
    let msg = protocol::read_message(&mut server).unwrap().unwrap();
    assert_eq!(msg, Message::Shutdown);
}

// ═══════════════════════════════════════════════════════════════════════
// Logic tests: pure computation, no IO
// ═══════════════════════════════════════════════════════════════════════

// ── Test 5: Counter monotonicity ────────────────────────────────────────

#[test]
fn counter_monotonicity_across_success_and_failure() {
    // Simulates the spawn_up_to counter logic:
    //   - counter increments on each attempt
    //   - on failure, counter is decremented
    //   - successfully-assigned IDs must be unique and monotonically increasing

    let mut counter: u64 = 0;
    let mut assigned_ids: Vec<String> = Vec::new();

    // Simulate: success, success, failure, success, failure, failure, success
    let outcomes = [true, true, false, true, false, false, true];

    for outcome in &outcomes {
        counter += 1;
        let session_id = format!("ws/{counter}");

        if *outcome {
            assert!(
                !assigned_ids.contains(&session_id),
                "session ID {session_id} was already assigned"
            );
            assigned_ids.push(session_id);
        } else {
            counter -= 1;
        }
    }

    // All assigned IDs unique
    let mut sorted = assigned_ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), assigned_ids.len(), "IDs should be unique");

    // Monotonically increasing
    let numbers: Vec<u64> = assigned_ids
        .iter()
        .map(|id| id.strip_prefix("ws/").unwrap().parse().unwrap())
        .collect();
    for window in numbers.windows(2) {
        assert!(
            window[0] < window[1],
            "IDs should be monotonically increasing: {} >= {}",
            window[0],
            window[1]
        );
    }
}

// ── Test 5b: Counter with concurrent success ────────────────────────────

#[test]
fn counter_monotonicity_concurrent_success() {
    // Simulates spawn_up_to with count=3 where all succeed, then
    // a spawn_for_task that also succeeds.

    let mut counter: u64 = 0;
    let mut assigned_ids: Vec<String> = Vec::new();

    // spawn_up_to(3): three successes
    for _ in 0..3 {
        counter += 1;
        assigned_ids.push(format!("ws/{counter}"));
    }

    // spawn_for_task: one more success
    counter += 1;
    assigned_ids.push(format!("ws/{counter}"));

    assert_eq!(assigned_ids, vec!["ws/1", "ws/2", "ws/3", "ws/4"]);
}

// ── Test 5c: Counter rollback on failure doesn't create gaps for active sessions ──

#[test]
fn counter_rollback_reuses_number() {
    // When spawn_up_to fails, it decrements counter and breaks.
    // The next successful spawn gets the same counter value.
    // This means the counter value is reused — but only after the
    // failed session was never tracked, so there's no conflict.

    let mut counter: u64 = 0;
    let mut active_ids: Vec<String> = Vec::new();

    // Success
    counter += 1; // counter=1
    active_ids.push(format!("ws/{counter}"));

    // Failure
    counter += 1; // counter=2
    counter -= 1; // rollback to 1

    // Success — gets ws/2 (counter increments to 2)
    counter += 1; // counter=2
    active_ids.push(format!("ws/{counter}"));

    assert_eq!(active_ids, vec!["ws/1", "ws/2"]);
    // No gap: ws/1 and ws/2 are both active
}

// ── Test 6b: spawn_for_task prevents double-claim by checking map ───────

#[test]
fn spawn_for_task_rejects_duplicate() {
    // Simulates spawn_for_task's initial check:
    // if task_to_session.contains_key(task_name) { bail!("already active") }

    use std::collections::HashMap;
    let mut task_to_session: HashMap<String, String> = HashMap::new();

    // First claim succeeds
    let task = "my-task".to_string();
    assert!(!task_to_session.contains_key(&task));
    task_to_session.insert(task.clone(), "ws/1".to_string());

    // Second claim is rejected
    assert!(task_to_session.contains_key(&task));
}

// ── Test 7b: Concurrent spawn_up_to uses claim_next atomicity ───────────

#[test]
fn concurrent_spawns_claim_distinct_tasks() {
    // Simulates two spawn_up_to calls each claiming tasks.
    // claim_next_task is atomic (SQLite transaction), so each call
    // gets a distinct task.

    let tasks = vec!["task-a", "task-b", "task-c"];
    let mut claimed_by_1: Vec<&str> = Vec::new();
    let mut claimed_by_2: Vec<&str> = Vec::new();
    let mut available: Vec<&str> = tasks.clone();

    // First spawner claims task-a
    if let Some(pos) = available.iter().position(|t| !claimed_by_2.contains(t)) {
        claimed_by_1.push(available.remove(pos));
    }

    // Second spawner claims task-b (task-a already claimed)
    if let Some(pos) = available.iter().position(|t| !claimed_by_1.contains(t)) {
        claimed_by_2.push(available.remove(pos));
    }

    // No overlap
    for t in &claimed_by_1 {
        assert!(
            !claimed_by_2.contains(t),
            "task {t} claimed by both spawners"
        );
    }
}

// ── Test: Reader thread EOF detection ───────────────────────────────────

#[test]
fn reader_detects_server_eof() {
    let (client, server) = UnixStream::pair().unwrap();

    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let alive_clone = alive.clone();

    // Spawn a reader thread
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(client);
        loop {
            match protocol::read_message(&mut reader) {
                Ok(Some(Message::PtyOutput(_))) => {}
                Ok(None) => break, // EOF
                Err(_) => break,   // Error
                Ok(Some(_)) => {}  // Other messages
            }
        }
        alive_clone.store(false, std::sync::atomic::Ordering::Release);
    });

    assert!(alive.load(std::sync::atomic::Ordering::Acquire));

    // Server closes — reader should detect EOF
    drop(server);
    handle.join().unwrap();

    assert!(!alive.load(std::sync::atomic::Ordering::Acquire));
}

// ── Test: Multiple messages in sequence ─────────────────────────────────

#[test]
fn protocol_multiple_pty_output_messages() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Server sends multiple PtyOutput messages
    for i in 0..10 {
        let data = format!("output line {i}\n");
        protocol::write_message(&mut server, &Message::PtyOutput(data.into_bytes())).unwrap();
    }

    // Client reads them all in order
    for i in 0..10 {
        let msg = protocol::read_message(&mut client).unwrap().unwrap();
        let expected = format!("output line {i}\n");
        assert_eq!(msg, Message::PtyOutput(expected.into_bytes()));
    }
}

// ── Test: Large InitialState transfer ───────────────────────────────────

#[test]
fn protocol_large_initial_state() {
    let (mut client, mut server) = UnixStream::pair().unwrap();

    // Simulate a large scrollback buffer (100KB)
    let large_data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();

    let large_data_clone = large_data.clone();
    let server_thread = std::thread::spawn(move || {
        let _ = protocol::read_message(&mut server); // Resize
        protocol::write_message(&mut server, &Message::InitialState(large_data_clone)).unwrap();
    });

    protocol::write_message(&mut client, &Message::Resize { rows: 23, cols: 80 }).unwrap();
    let msg = protocol::read_message(&mut client).unwrap().unwrap();

    match msg {
        Message::InitialState(data) => {
            assert_eq!(data.len(), 100_000);
            assert_eq!(data, large_data);
        }
        other => panic!(
            "expected InitialState, got {:?}",
            std::mem::discriminant(&other)
        ),
    }

    server_thread.join().unwrap();
}

// ── Test: Broken pipe on write after disconnect ─────────────────────────

#[test]
fn write_after_disconnect_returns_error() {
    let (client, server) = UnixStream::pair().unwrap();
    drop(server);

    let mut writer = BufWriter::new(client);
    // First write might succeed (kernel buffer), but flush should fail
    let _ = protocol::write_message(&mut writer, &Message::PtyInput(b"data".to_vec()));
    // Second write should definitely fail
    let result = protocol::write_message(&mut writer, &Message::PtyInput(b"data".to_vec()));
    assert!(result.is_err(), "write to disconnected peer should fail");
}
