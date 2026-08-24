//! comms-orch#11 slice B: proves the wiring in the real `clerk` binary, not just src/logging.rs
//! in isolation. Spawns the REAL binary as a throwaway subprocess, drives it through the SAME
//! code path production uses, and asserts on the real log file it writes.
//!
//! Boundary from Overwatch's gate-1 on slice A, carried over here: the clerks running for real
//! seats are operator-owned. This file never touches one. It starts a throwaway clerk process
//! with an isolated CLERK_LOG_DIR and a RELAY_URL that points at a closed local port, so the
//! process retries its connection forever (connect_with_backoff has no max_attempts here) and
//! never touches a real relay or a real seat's state.
//!
//! Lifecycle discipline: this is the exact bug agencyos-compact-driver's slice A test fixture
//! had (a spawned child only got killed on the test's own success path, so a mutation that made
//! waitFor() throw first leaked the child and hung `node --test` for two hours). ChildGuard's
//! Drop impl makes that class of bug structurally impossible here: whatever this file spawns is
//! reaped in Drop, unconditionally, on success, on assertion failure, and on panic alike.
//!
//! CD-1 (MUTATION TARGET): the real binary writes a startup banner (commit, pid, seat identity,
//!   UTC ts) to the durable log file, keyed on the seat identity CLERK_LOG_DIR / SEAT_ROLE
//!   determine -- proves clerk_log_path and the wiring in main() together, not the function
//!   in isolation.
//! CD-2 (MUTATION TARGET, AC1 SIGTERM half): a real SIGTERM against the throwaway process is
//!   logged before it exits, and the process actually exits (code 0).
//! CD-3: SIGKILL against the throwaway process leaves the log stopped abruptly with no
//!   graceful-exit line -- the same honest, unavoidable gap slice A documents, not a bug here.
//!
//! Mutation discipline: comment out the SIGTERM/SIGINT signal-handling spawn block in
//! bin/clerk.rs's main() and CD-2 goes red by name (the log never records the SIGTERM). Comment
//! out the startup-banner append_log_line call and CD-1 goes red by name. Mutate the call site,
//! never src/logging.rs's functions themselves.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HARD_TIMEOUT: Duration = Duration::from_secs(15);
const TEST_NSEC: &str = "nsec1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqstywftw";

/// Kills and reaps whatever child this guard wraps, unconditionally, in Drop -- so a real
/// spawned process can never outlive the test that started it, regardless of which code path
/// the test body took (assertion failure, timeout, panic, or a clean return all run Drop).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_throwaway_clerk(tmp_dir: &std::path::Path, seat_role: &str) -> ChildGuard {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_clerk"));
    let child = Command::new(bin)
        .env("SEAT_NSEC", TEST_NSEC)
        // A closed local port: connect_with_backoff retries forever without ever reaching a
        // real relay, which is exactly the "stays alive, does nothing live" shape this test
        // needs -- never a real relay, never a real seat.
        .env("RELAY_URL", "ws://127.0.0.1:1")
        .env("CLERK_LOG_DIR", tmp_dir)
        .env("SEAT_ROLE", seat_role)
        .env(
            "WAKE_FILE",
            tmp_dir.join("wake").to_string_lossy().to_string(),
        )
        .env(
            "IDENTITY_FILE",
            tmp_dir.join("identity.json").to_string_lossy().to_string(),
        )
        .env(
            "READACK_FILE",
            tmp_dir.join("readack").to_string_lossy().to_string(),
        )
        .env("CLAIM_DIR", tmp_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn throwaway clerk binary");
    ChildGuard(child)
}

fn wait_for(mut predicate: impl FnMut() -> bool, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if predicate() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err("wait_for: timed out waiting for condition".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_log(path: &std::path::Path) -> String {
    let mut s = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read_to_string(&mut s);
    }
    s
}

#[test]
fn cd1_mutation_target_startup_banner_written_by_the_real_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("buzz-seat-clerk-co11b-cd1.log");
    let guard = spawn_throwaway_clerk(tmp.path(), "co11b-cd1");

    wait_for(
        || log_path.exists() && read_log(&log_path).contains("STARTUP"),
        HARD_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("{e}; log contents so far: {:?}", read_log(&log_path)));

    let content = read_log(&log_path);
    assert!(
        content.contains("seat=co11b-cd1"),
        "MUTATION TARGET: the seat identity must be on the banner line, got:\n{content}"
    );
    assert!(
        content.contains(&format!("pid={}", guard.0.id())),
        "MUTATION TARGET: the logged pid must be THIS real process's own pid, got:\n{content}"
    );
    assert!(
        content.contains("STARTUP commit="),
        "MUTATION TARGET: expected a real startup banner, got:\n{content}"
    );
}

#[test]
fn cd2_mutation_target_sigterm_is_logged_before_the_process_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("buzz-seat-clerk-co11b-cd2.log");
    let mut guard = spawn_throwaway_clerk(tmp.path(), "co11b-cd2");

    wait_for(
        || log_path.exists() && read_log(&log_path).contains("STARTUP"),
        HARD_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("{e}; log contents so far: {:?}", read_log(&log_path)));

    // Sends a real SIGTERM to the throwaway process THIS test spawned above, by its own
    // captured pid -- never a pattern-based kill, never a real fleet clerk.
    let pid = guard.0.id();
    send_signal(pid, "TERM");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = guard.0.try_wait().expect("try_wait failed") {
            break status;
        }
        if start.elapsed() > HARD_TIMEOUT {
            panic!("process did not exit after SIGTERM within the hard timeout");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let content = read_log(&log_path);
    assert!(
        content.contains("received SIGTERM, exiting gracefully"),
        "MUTATION TARGET: the log must record the real SIGTERM before exit, got:\n{content}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "a SIGTERM-triggered graceful shutdown must exit 0"
    );
}

#[test]
fn cd3_documents_the_real_sigkill_limit_not_a_bug() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("buzz-seat-clerk-co11b-cd3.log");
    let mut guard = spawn_throwaway_clerk(tmp.path(), "co11b-cd3");

    wait_for(
        || log_path.exists() && read_log(&log_path).contains("STARTUP"),
        HARD_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("{e}; log contents so far: {:?}", read_log(&log_path)));

    let pid = guard.0.id();
    send_signal(pid, "KILL");

    let start = Instant::now();
    loop {
        if guard.0.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        if start.elapsed() > HARD_TIMEOUT {
            panic!("process did not exit after SIGKILL within the hard timeout");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let content = read_log(&log_path);
    assert!(
        !content.contains("received SIG"),
        "SIGKILL cannot be caught by any process -- no graceful-exit line can ever appear for this case, by construction, not a bug in this code: {content}"
    );
    assert!(
        content.contains("STARTUP"),
        "the log must still show the process WAS alive and logging up until the kill: {content}"
    );
}

/// Sends `signal` (e.g. `"TERM"`, `"KILL"`) to `pid` via the real `kill` binary, matching this
/// crate's `#![deny(unsafe_code)]` posture (no raw libc FFI, not even in tests) instead of
/// binding the syscall directly for three call sites.
fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .expect("failed to invoke kill(1)");
    assert!(status.success(), "kill -s {signal} {pid} failed");
}
