//! Integration test for the Windows Job Object adapter's tree-kill guarantee.
//!
//! The whole point of `ProcessSandbox` is that killing the direct child is NOT
//! enough — `git`/`npm`/`cargo` spawn grandchildren that must also die. This test
//! proves *grandchild* death, not merely direct-child death:
//!
//! 1. spawn `cmd /c ping -n 1000 127.0.0.1`
//!    → `cmd.exe` is the direct child, `ping.exe` is a long-lived **grandchild**.
//! 2. discover the grandchild PID (a `ping.exe` whose ParentProcessId == cmd's PID)
//!    and confirm it is running.
//! 3. call `kill_tree()`.
//! 4. assert that *specific* grandchild PID is gone — i.e. the whole job died.
//!
//! Step 2 pins the exact grandchild by parent PID so we never accidentally observe
//! an unrelated `ping.exe`; step 4 is what would FAIL if only the direct child were
//! killed (`tokio`'s `kill_on_drop` behaviour).

#![cfg(windows)]

use std::time::Duration;

use process_sandbox::{platform_sandbox, OutputPolicy, ToolSpec};

/// Return PIDs of processes whose parent is `parent_pid` and whose image name
/// matches `image` (case-insensitive), via a single CIM query.
fn child_pids(parent_pid: u32, image: &str) -> Vec<u32> {
    // One PowerShell/CIM call. Robust across modern Windows where wmic is removed.
    let script = format!(
        "Get-CimInstance Win32_Process -Filter 'ParentProcessId={parent_pid}' \
         | Where-Object {{ $_.Name -ieq '{image}' }} \
         | ForEach-Object {{ $_.ProcessId }}"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("failed to run powershell CIM query");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// `true` if a process with `pid` currently exists. Uses `tasklist`, which is
/// fast and always present.
fn pid_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("failed to run tasklist");
    let text = String::from_utf8_lossy(&output.stdout);
    // tasklist prints a CSV row containing the PID when present; otherwise it
    // prints an "INFO: No tasks..." line (or nothing useful).
    text.contains(&format!("\"{pid}\""))
}

/// Poll `f` until it returns `true` or the deadline elapses.
async fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_tree_terminates_grandchild() {
    let sandbox = platform_sandbox();

    // cmd.exe (child) launches ping.exe (grandchild) that runs ~1000s.
    let spec = ToolSpec::new(
        "cmd",
        ["/c", "ping", "-n", "1000", "127.0.0.1"],
    );
    let mut proc = sandbox.spawn(spec).await.expect("spawn failed");
    let child_pid = proc.pid();
    assert!(child_pid > 0, "expected a real PID");

    // Drain output concurrently so the child never blocks on a full pipe while we
    // poll (the OUTPUT_AND_IPC.md invariant). We don't assert on its contents.
    let stdout = proc.take_stdout();
    let stderr = proc.take_stderr();
    let drain = tokio::spawn(async move {
        let _ = process_sandbox::drain_parallel(stdout, stderr, OutputPolicy::default()).await;
    });

    // 1) Find the grandchild ping.exe whose parent is our cmd.exe, and confirm it
    //    is actually running. Give Windows a moment to spawn it.
    let mut grandchild_pid = None;
    let found = wait_until(
        || match child_pids(child_pid, "ping.exe").first().copied() {
            Some(pid) => {
                grandchild_pid = Some(pid);
                true
            }
            None => false,
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        found,
        "did not observe a ping.exe grandchild under cmd.exe pid {child_pid}; \
         cannot prove tree-kill"
    );
    let grandchild_pid = grandchild_pid.unwrap();
    assert!(
        pid_alive(grandchild_pid),
        "grandchild ping.exe (pid {grandchild_pid}) should be alive before kill"
    );

    // 2) Kill the whole tree via the Job Object.
    proc.kill_tree().await.expect("kill_tree failed");

    // 3) The grandchild must be gone. This is the assertion that fails if only the
    //    direct child were killed.
    let dead = wait_until(|| !pid_alive(grandchild_pid), Duration::from_secs(10)).await;
    assert!(
        dead,
        "grandchild ping.exe (pid {grandchild_pid}) survived kill_tree() — \
         the process tree was NOT killed"
    );

    // The direct child should also be gone.
    assert!(
        !pid_alive(child_pid),
        "direct child cmd.exe (pid {child_pid}) survived kill_tree()"
    );

    // wait() after a tree-kill must resolve (and be idempotent).
    let _ = proc.wait().await;
    let _ = drain.await;
}

/// A second, lighter check: spawn, capture output, and confirm a clean exit path
/// works end-to-end through the real adapter (not just kill).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_capture_and_wait_clean_exit() {
    let sandbox = platform_sandbox();
    let mut proc = sandbox
        .spawn(ToolSpec::new("cmd", ["/c", "echo", "hello-sandbox"]))
        .await
        .expect("spawn failed");

    let stdout = proc.take_stdout();
    let stderr = proc.take_stderr();
    let drained =
        tokio::spawn(
            async move { process_sandbox::drain_parallel(stdout, stderr, OutputPolicy::default()).await },
        );

    let exit = proc.wait().await.expect("wait failed");
    assert!(exit.is_success(), "echo should exit 0, got {exit:?}");

    let (out, _err) = drained.await.unwrap().unwrap();
    let out = out.unwrap().into_string_lossy();
    assert!(
        out.contains("hello-sandbox"),
        "expected echoed text in stdout, got: {out:?}"
    );
}
