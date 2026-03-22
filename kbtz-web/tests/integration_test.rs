use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use kbtz_web::protocol::{self, Message, ShepherdState};

/// Path to the compiled kbtz-json-shepherd binary.
fn shepherd_bin() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("kbtz-json-shepherd");
    path
}

/// Spawn the shepherd binary, returning the child process and a File for the
/// readiness pipe read end. Uses `pre_exec` to force fork+exec (instead of
/// posix_spawn with CLOSEFROM) so the ready-fd survives into the child.
fn spawn_shepherd(
    socket_path: &std::path::Path,
    state_path: &std::path::Path,
    session_id: &str,
    event_cap: &str,
    mock_script: &std::path::Path,
) -> (std::process::Child, std::fs::File) {
    let (read_fd, write_fd) = {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    };
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write_fd, libc::F_SETFD, 0);
    }

    let mut cmd = Command::new(shepherd_bin());
    cmd.args([
        socket_path.to_str().unwrap(),
        state_path.to_str().unwrap(),
        session_id,
        event_cap,
        "--ready-fd",
        &write_fd.to_string(),
        mock_script.to_str().unwrap(),
    ]);
    // pre_exec forces fork+exec instead of posix_spawn, so fds without
    // CLOEXEC (our write_fd) survive into the child process.
    unsafe {
        let wfd = write_fd;
        cmd.pre_exec(move || {
            libc::fcntl(wfd, libc::F_SETFD, 0);
            Ok(())
        });
    }

    let child = cmd.spawn().expect("failed to spawn kbtz-json-shepherd");
    // Close write end in parent.
    unsafe { libc::close(write_fd) };

    let read_pipe: std::fs::File = unsafe { std::os::unix::io::FromRawFd::from_raw_fd(read_fd) };
    (child, read_pipe)
}

/// Wait for the readiness byte on the pipe.
fn wait_ready(mut pipe: std::fs::File) {
    use std::io::Read;
    let mut buf = [0u8; 1];
    let n = pipe.read(&mut buf).expect("pipe read failed");
    assert_eq!(n, 1, "expected readiness byte from shepherd");
    assert_eq!(buf[0], 0x01);
}

#[test]
fn shepherd_spawn_connect_events_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("shepherd.sock");
    let state_path = dir.path().join("state.json");
    let session_id = "web/test-1";

    let mock_script = dir.path().join("mock-agent.sh");
    std::fs::write(
        &mock_script,
        r#"#!/bin/bash
echo '{"type":"start","message":"hello"}'
echo '{"type":"progress","step":1}'
echo '{"type":"done","result":"ok"}'
# Wait for input
read -r line
if [ "$line" = "user-input" ]; then
    echo '{"type":"echo","input":"user-input"}'
fi
sleep 0.1
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &mock_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let (mut child, read_pipe) =
        spawn_shepherd(&socket_path, &state_path, session_id, "100", &mock_script);
    wait_ready(read_pipe);

    // State file should exist now.
    let state = ShepherdState::read_state_file(&state_path).unwrap();
    assert_eq!(state.session_id, session_id);
    assert!(state.child_pid.is_some());

    // Wait until the agent has produced its initial events.
    let start = Instant::now();
    loop {
        if let Ok(s) = ShepherdState::read_state_file(&state_path) {
            if s.event_count >= 3 {
                break;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for agent events"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Connect to the socket.
    let stream = UnixStream::connect(&socket_path).expect("failed to connect to shepherd socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    // Read the EventBatch (history replay).
    let msg = protocol::read_message(&mut reader)
        .expect("read EventBatch failed")
        .expect("expected EventBatch, got EOF");

    let events = match msg {
        Message::EventBatch { events } => events,
        other => panic!("expected EventBatch, got {:?}", other),
    };

    assert!(
        events.len() >= 3,
        "expected >= 3 events in batch, got {}",
        events.len()
    );

    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].data["type"], "start");
    assert_eq!(events[1].seq, 2);
    assert_eq!(events[1].data["type"], "progress");
    assert_eq!(events[2].seq, 3);
    assert_eq!(events[2].data["type"], "done");

    // Verify monotonic timestamps.
    for window in events.windows(2) {
        assert!(window[1].timestamp >= window[0].timestamp);
    }

    // Send input to the agent.
    let mut writer = stream.try_clone().unwrap();
    protocol::write_message(
        &mut writer,
        &Message::Input {
            data: "user-input\n".into(),
        },
    )
    .unwrap();

    // Read the echo event.
    let msg = protocol::read_message(&mut reader)
        .expect("read echo event failed")
        .expect("expected echo Event, got EOF");

    match msg {
        Message::Event { event } => {
            assert_eq!(event.data["type"], "echo");
            assert_eq!(event.data["input"], "user-input");
            assert_eq!(event.seq, 4);
        }
        other => panic!("expected Event, got {:?}", other),
    }

    // Wait for shepherd to exit (mock agent exits after sleep).
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success(),
                    "shepherd exited with non-zero: {:?}",
                    status
                );
                break;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    child.kill().unwrap();
                    panic!("shepherd did not exit within timeout");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait error: {e}"),
        }
    }

    // Socket and state files should be cleaned up.
    assert!(!socket_path.exists(), "socket should be cleaned up");
    assert!(!state_path.exists(), "state file should be cleaned up");
}

#[test]
fn shepherd_event_cap_limits_ring_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("shepherd.sock");
    let state_path = dir.path().join("state.json");

    let mock_script = dir.path().join("mock-agent.sh");
    std::fs::write(
        &mock_script,
        r#"#!/bin/bash
echo '{"n":1}'
echo '{"n":2}'
echo '{"n":3}'
echo '{"n":4}'
echo '{"n":5}'
sleep 0.1
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &mock_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let (mut child, read_pipe) =
        spawn_shepherd(&socket_path, &state_path, "web/test-cap", "3", &mock_script);
    wait_ready(read_pipe);

    // Wait until all 5 events have been processed by polling the state file.
    let start = Instant::now();
    loop {
        if let Ok(s) = ShepherdState::read_state_file(&state_path) {
            // last_seq == 5 means all 5 events were processed (even though
            // only 3 are kept in the ring buffer due to the cap).
            if s.last_seq >= 5 {
                break;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for all events"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Connect and get the event batch.
    let stream = UnixStream::connect(&socket_path).expect("failed to connect for event cap test");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);

    let msg = protocol::read_message(&mut reader)
        .expect("read EventBatch failed")
        .expect("expected EventBatch, got EOF");

    match msg {
        Message::EventBatch { events } => {
            assert_eq!(
                events.len(),
                3,
                "expected exactly 3 events due to cap, got {}",
                events.len()
            );
            // Ring buffer should contain the 3 most recent events.
            assert_eq!(events[0].data["n"], 3);
            assert_eq!(events[1].data["n"], 4);
            assert_eq!(events[2].data["n"], 5);
        }
        other => panic!("expected EventBatch, got {:?}", other),
    }

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    child.kill().unwrap();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

#[test]
fn shepherd_shutdown_message_kills_child() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("shepherd.sock");
    let state_path = dir.path().join("state.json");

    let mock_script = dir.path().join("mock-agent.sh");
    std::fs::write(
        &mock_script,
        r#"#!/bin/bash
echo '{"type":"started"}'
trap 'echo "{\"type\":\"shutdown\"}"; exit 0' TERM
while true; do sleep 1; done
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &mock_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let (mut child, read_pipe) = spawn_shepherd(
        &socket_path,
        &state_path,
        "web/test-shutdown",
        "100",
        &mock_script,
    );
    wait_ready(read_pipe);

    // Connect to shepherd.
    let stream = UnixStream::connect(&socket_path).expect("connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    // Read initial EventBatch.
    let msg = protocol::read_message(&mut reader).unwrap().unwrap();
    assert!(matches!(msg, Message::EventBatch { .. }));

    // Send Shutdown message.
    let mut writer = stream.try_clone().unwrap();
    protocol::write_message(&mut writer, &Message::Shutdown).unwrap();

    // Shepherd should exit within a few seconds.
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    child.kill().unwrap();
                    panic!("shepherd did not exit after Shutdown");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait error: {e}"),
        }
    }
}
