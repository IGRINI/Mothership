//! End-to-end protocol test against the real sidecar binary: spawn it, run the
//! Hello/Initialize/Ready handshake, round-trip a request, and shut down. This
//! exercises the whole daemon path (spawn -> DB open + migrate -> serve) over
//! actual stdio without the desktop GUI, and reports the IPC round-trip latency.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use mothership_core::ipc::{ClientFrame, CoreRequest, CoreResponse, ServerFrame};

#[test]
fn sidecar_handshake_and_dashboard_roundtrip() {
    let dir = std::env::temp_dir().join(format!("mothership_sidecar_it_{}", nanos()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let db_path = dir.join("mothership.sqlite3");

    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_mothership-sidecar"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn sidecar");

    let mut stdin = child.stdin.take().expect("sidecar stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("sidecar stdout"));

    // 1. The sidecar greets first.
    match read_frame(&mut reader) {
        ServerFrame::Hello { .. } => {}
        other => panic!("expected Hello, got {other:?}"),
    }

    // 2. We hand it the database path...
    write_frame(
        &mut stdin,
        &ClientFrame::Initialize {
            db_path: db_path.clone(),
        },
    );

    // 3. ...and it opens + migrates the DB, then signals readiness.
    match read_frame(&mut reader) {
        ServerFrame::Ready => {}
        other => panic!("expected Ready, got {other:?}"),
    }

    // 4. A correlated request returns its single terminal response.
    let started = Instant::now();
    write_frame(
        &mut stdin,
        &ClientFrame::Request {
            id: 1,
            request: CoreRequest::DashboardSnapshot,
        },
    );
    match read_frame(&mut reader) {
        ServerFrame::Response { id, result } => {
            assert_eq!(id, 1, "response id must match the request");
            assert!(
                matches!(result, CoreResponse::Dashboard(_)),
                "expected a Dashboard response, got {result:?}"
            );
        }
        other => panic!("expected Response, got {other:?}"),
    }
    let elapsed = started.elapsed();
    println!("sidecar dashboard round-trip latency: {elapsed:?}");
    assert!(
        elapsed.as_millis() < 200,
        "round-trip latency unexpectedly high: {elapsed:?}"
    );

    // 5. The connector path runs Core's adapter logic inside the sidecar. With no
    // plugins in this temp dir it returns an empty provider list — the point is
    // that ConnectorService executes in-process without panicking over IPC.
    write_frame(
        &mut stdin,
        &ClientFrame::Request {
            id: 2,
            request: CoreRequest::ConnectorSettings,
        },
    );
    match read_frame(&mut reader) {
        ServerFrame::Response { id, result } => {
            assert_eq!(id, 2);
            assert!(
                matches!(result, CoreResponse::ConnectorSettings(_)),
                "expected ConnectorSettings, got {result:?}"
            );
        }
        other => panic!("expected Response, got {other:?}"),
    }

    // 6. Clean shutdown on request.
    write_frame(&mut stdin, &ClientFrame::Shutdown);
    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads the next non-empty line and decodes it as a [`ServerFrame`].
fn read_frame(reader: &mut impl BufRead) -> ServerFrame {
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read sidecar stdout");
        assert_ne!(read, 0, "sidecar closed stdout before a frame arrived");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .unwrap_or_else(|error| panic!("decode frame {trimmed:?}: {error}"));
    }
}

fn write_frame(stdin: &mut ChildStdin, frame: &ClientFrame) {
    let mut line = serde_json::to_string(frame).expect("encode frame");
    line.push('\n');
    stdin.write_all(line.as_bytes()).expect("write to sidecar");
    stdin.flush().expect("flush sidecar stdin");
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos()
}
