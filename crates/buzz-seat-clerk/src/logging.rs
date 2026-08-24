//! comms-orch#11 slice B: a durable, size-capped, per-seat-attributable log file for the
//! clerk binary.
//!
//! Design constraints from the ticket, mirrored from slice A (agencyos-compact-driver's
//! src/log-file.js) with two clerk-specific steers from gate-1:
//!   - Seat identity must be discoverable from the filename OR present on every line, because
//!     when several clerks die at once (a fleet-wide relay outage, a bad deploy) an operator
//!     grepping /tmp needs to tell them apart without cross-referencing a PID table. This module
//!     does both: `clerk_log_path` puts the identity in the filename, and `append_log_line`
//!     puts it on every line too, so a log file that got renamed or a `cat *.log` merge never
//!     loses attribution.
//!   - The path is discoverable through a function, `clerk_log_path`, never a hardcoded string,
//!     so a consumer (comms-orch#10's watcher) can compute it the same way this binary does.
//!
//! SIGKILL cannot be caught by any process on any OS -- documented here, not silently gapped.
//! That half of "why did it die" can only come from whatever supervises the clerk process.

use std::fs;
use std::path::Path;
use std::process::Command;

const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB
const DEFAULT_MAX_ROTATIONS: u32 = 3;

/// Directory the clerk's log file lives in. `CLERK_LOG_DIR` lets an integration test redirect
/// every clerk log write to an isolated tmpdir, mirroring `COMPACT_DRIVER_TMP` in the sibling
/// compact-driver repo -- never touching a real clerk's own log file.
pub fn clerk_log_dir() -> String {
    std::env::var("CLERK_LOG_DIR").unwrap_or_else(|_| "/tmp".to_string())
}

/// Strips path-traversal and separator characters from a seat identity before it is used to
/// build a filename. Seat role strings originate from fleet config, not attacker input, but a
/// filename builder that trusts its input unconditionally is a footgun waiting for a future
/// caller with less trustworthy data -- cheap to guard now.
fn sanitize_identity(identity: &str) -> String {
    identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The discoverable log path for one clerk instance. `identity` should be the seat role when
/// known (e.g. `"AgencyOS-CC-Alpha"`) or a short pubkey prefix when it is not -- callers pass
/// whichever `resolve_seat_identity` returns, this function never chooses on its own.
pub fn clerk_log_path(identity: &str) -> String {
    format!(
        "{}/buzz-seat-clerk-{}.log",
        clerk_log_dir(),
        sanitize_identity(identity)
    )
}

/// The seat identity used for both the log filename and each log line: `seat_role` when the
/// fleet config set one, otherwise the first 8 hex chars of the seat's own pubkey (always
/// present) so a clerk started without SEAT_ROLE still produces an attributable log.
pub fn resolve_seat_identity(seat_role: Option<&str>, public_key_hex: &str) -> String {
    match seat_role {
        Some(role) if !role.is_empty() => role.to_string(),
        _ => public_key_hex.chars().take(8).collect(),
    }
}

/// Appends one line to the log file, rotating first if the file is already at or over
/// `max_bytes`. Rotation shifts `path -> path.1 -> path.2 ...` up to `max_rotations`, then drops
/// the oldest -- bounded disk use regardless of how long the clerk (or a crash loop) runs.
/// Best-effort: a logging failure must never crash the clerk it is trying to observe.
pub fn append_log_line(path: &str, seat_identity: &str, line: &str) {
    append_log_line_with(
        path,
        seat_identity,
        line,
        DEFAULT_MAX_BYTES,
        DEFAULT_MAX_ROTATIONS,
    );
}

/// Same as [`append_log_line`] with injectable rotation limits, so tests can force a rotation
/// without writing 5 MiB of fixture data.
pub fn append_log_line_with(
    path: &str,
    seat_identity: &str,
    line: &str,
    max_bytes: u64,
    max_rotations: u32,
) {
    rotate_if_needed(path, max_bytes, max_rotations);
    let ts = chrono::Utc::now().to_rfc3339();
    let formatted = format!("[{ts}] seat={seat_identity} {line}\n");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(formatted.as_bytes())
        });
}

/// Rotation: if `path` exists and is >= `max_bytes`, shift it through `path.1..path.N`
/// (dropping the oldest past `max_rotations`), then leave `path` itself absent so the next
/// append starts a fresh file. Pure filesystem shuffling, no content inspection.
fn rotate_if_needed(path: &str, max_bytes: u64, max_rotations: u32) {
    let size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return, // file does not exist yet: nothing to rotate
    };
    if size < max_bytes {
        return;
    }
    let oldest = format!("{path}.{max_rotations}");
    let _ = fs::remove_file(&oldest);
    let mut i = max_rotations.saturating_sub(1);
    while i >= 1 {
        let from = format!("{path}.{i}");
        let to = format!("{path}.{}", i + 1);
        let _ = fs::rename(&from, &to);
        i -= 1;
    }
    let _ = fs::rename(path, format!("{path}.1"));
}

/// The startup banner line: commit loaded, pid, seat identity, UTC timestamp -- the same four
/// fields slice A's `formatStartupBanner` writes, so a mixed compact-driver/clerk log sweep
/// reads consistently.
pub fn format_startup_banner(commit: &str, pid: u32, seat_identity: &str) -> String {
    let ts = chrono::Utc::now().to_rfc3339();
    format!("STARTUP commit={commit} pid={pid} seat={seat_identity} ts={ts}")
}

/// Resolves the git commit this process is running from via a real `git rev-parse HEAD` call,
/// the one way this process ever asks "what commit am I." Returns `"unknown"` on any failure
/// (detached checkout, no git binary, not a git repo) rather than crashing the clerk over a
/// cosmetic startup-banner field.
pub fn resolve_git_commit(repo_dir: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        // Overwatch gate-1 bounce (REV-20260823-01 class, same root as
        // scripts/install-clerk.sh's b79467e15 fix in this same repo): `.current_dir()` only
        // changes the working DIRECTORY. It does NOT override GIT_DIR/GIT_WORK_TREE/etc when
        // those are ambient in the environment, and git sets GIT_DIR for every hook it invokes
        // -- including this repo's own pre-push hook, which is exactly where this call was
        // caught reading the WRONG repo's HEAD. This binary has no legitimate reason to inherit
        // any of these; scrub them unconditionally so current_dir is actually authoritative.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // Serializes tests that mutate CLERK_LOG_DIR, matching config.rs's ENV_LOCK convention:
    // env vars are process-global and the test harness runs tests in parallel by default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn append_log_line_writes_a_timestamped_seat_attributed_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        append_log_line(path.to_str().unwrap(), "AgencyOS-CC-Alpha", "hello world");
        let content = read(&path);
        assert!(
            content.contains("seat=AgencyOS-CC-Alpha"),
            "every line must carry the seat identity, got: {content}"
        );
        assert!(content.contains("hello world"));
        assert!(
            content.starts_with('['),
            "line must start with a bracketed timestamp, got: {content}"
        );
    }

    #[test]
    fn append_log_line_appends_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        append_log_line(path.to_str().unwrap(), "seat-a", "line one");
        append_log_line(path.to_str().unwrap(), "seat-a", "line two");
        let content = read(&path);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "both lines must be present: {content}");
        assert!(lines[0].ends_with("line one"));
        assert!(lines[1].ends_with("line two"));
    }

    #[test]
    fn append_log_line_is_best_effort_never_panics_on_bad_path() {
        // A path under a directory that does not exist: OpenOptions will fail; must not panic.
        append_log_line("/nonexistent-co11b-dir/clerk.log", "seat-a", "x");
    }

    #[test]
    fn rotation_mutation_target_rotates_at_or_over_cap_preserving_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        fs::write(&path, "x".repeat(100)).unwrap();
        append_log_line_with(path.to_str().unwrap(), "seat-a", "fresh line", 100, 3);
        let rotated = read(&dir.path().join("clerk.log.1"));
        assert_eq!(
            rotated,
            "x".repeat(100),
            "MUTATION TARGET: old content must be preserved under .1, not dropped"
        );
        let fresh = read(&path);
        assert!(
            fresh.contains("fresh line"),
            "the new line must land in a FRESH file: {fresh}"
        );
        assert!(
            !fresh.starts_with('x'),
            "the fresh file must not still contain the old content: {fresh}"
        );
    }

    #[test]
    fn rotation_never_fires_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        fs::write(&path, "short").unwrap();
        append_log_line_with(path.to_str().unwrap(), "seat-a", "more", 1000, 3);
        assert!(
            !dir.path().join("clerk.log.1").exists(),
            "must not rotate a file well under the cap"
        );
    }

    #[test]
    fn rotation_shifts_existing_backups_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        fs::write(&path, "x".repeat(100)).unwrap();
        fs::write(dir.path().join("clerk.log.1"), "gen-1").unwrap();
        fs::write(dir.path().join("clerk.log.2"), "gen-2").unwrap();
        append_log_line_with(path.to_str().unwrap(), "seat-a", "new", 100, 3);
        assert_eq!(read(&dir.path().join("clerk.log.1")), "x".repeat(100));
        assert_eq!(read(&dir.path().join("clerk.log.2")), "gen-1");
        assert_eq!(read(&dir.path().join("clerk.log.3")), "gen-2");
    }

    #[test]
    fn rotation_mutation_target_drops_oldest_past_max_rotations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clerk.log");
        fs::write(&path, "x".repeat(100)).unwrap();
        fs::write(dir.path().join("clerk.log.1"), "gen-1").unwrap();
        fs::write(dir.path().join("clerk.log.2"), "gen-2").unwrap();
        fs::write(dir.path().join("clerk.log.3"), "gen-3-should-be-dropped").unwrap();
        append_log_line_with(path.to_str().unwrap(), "seat-a", "new", 100, 3);
        let gen3 = read(&dir.path().join("clerk.log.3"));
        assert_eq!(
            gen3, "gen-2",
            "MUTATION TARGET: the pre-existing oldest backup must be gone, replaced by gen-2 shifting in, not silently kept forever (unbounded disk use if this no-ops)"
        );
        assert!(!dir.path().join("clerk.log.4").exists());
    }

    #[test]
    fn resolve_seat_identity_prefers_seat_role() {
        assert_eq!(
            resolve_seat_identity(Some("AgencyOS-CC-Alpha"), "deadbeef1234"),
            "AgencyOS-CC-Alpha"
        );
    }

    #[test]
    fn resolve_seat_identity_falls_back_to_pubkey_prefix_when_role_absent() {
        assert_eq!(resolve_seat_identity(None, "deadbeef1234"), "deadbeef");
    }

    #[test]
    fn resolve_seat_identity_falls_back_when_role_is_empty_string() {
        assert_eq!(resolve_seat_identity(Some(""), "deadbeef1234"), "deadbeef");
    }

    #[test]
    fn clerk_log_path_sanitizes_traversal_characters() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLERK_LOG_DIR", "/tmp/co11b-sanitize-test");
        let path = clerk_log_path("../../etc/passwd");
        assert!(
            !path.contains(".."),
            "MUTATION TARGET: traversal sequences must be scrubbed from the filename: {path}"
        );
        assert!(!path.contains('/') || path.starts_with("/tmp/co11b-sanitize-test/"));
        std::env::remove_var("CLERK_LOG_DIR");
    }

    #[test]
    fn format_startup_banner_mutation_target_names_every_field() {
        let banner = format_startup_banner("abc1234", 4242, "AgencyOS-CC-Alpha");
        assert!(banner.contains("commit=abc1234"), "{banner}");
        assert!(banner.contains("pid=4242"), "{banner}");
        assert!(banner.contains("seat=AgencyOS-CC-Alpha"), "{banner}");
        assert!(banner.starts_with("STARTUP "), "{banner}");
    }

    #[test]
    fn resolve_git_commit_returns_unknown_for_a_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let commit = resolve_git_commit(dir.path());
        assert_eq!(commit, "unknown");
    }

    // Overwatch gate-1 bounce: the test above only ever passed because the CI/dev-terminal
    // environment happened to have no ambient GIT_DIR -- it was testing "nobody set GIT_DIR",
    // not the fallback itself. This repo's own pre-push hook sets GIT_DIR for every hook it
    // runs, and under that ambient env resolve_git_commit silently returned the PUSHED repo's
    // real HEAD instead of "unknown" for a directory that is not a git repo at all -- a
    // plausible wrong sha, strictly worse than an honest "unknown", on the exact field the
    // clerk startup banner exists to answer ("what code am I running"). This test sets GIT_DIR
    // DELIBERATELY to a real (throwaway, self-contained) repo and proves the fallback still
    // fires for an unrelated non-git directory -- the actual effect the env_remove calls above
    // exist to guarantee, not just "the fallback path is reachable somehow".
    #[test]
    fn resolve_git_commit_mutation_target_ignores_ambient_git_dir_for_a_non_git_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        let decoy = tempfile::tempdir().unwrap();
        // This fixture's OWN setup calls need the exact same env_remove hardening as
        // resolve_git_commit itself: this test proved that live, the hard way. Running this
        // suite under the repo's real pre-push hook (which sets GIT_DIR ambiently for the whole
        // process, including every test) with an unguarded `run()` here caused these setup
        // commands to run against the REAL enclosing repo's object database instead of the decoy
        // tempdir -- `git add -A; git commit` staged and committed against the ACTUAL git
        // directory while cwd (and therefore the effective work tree) was this decoy dir,
        // corrupting the real branch with a commit whose tree held only decoy.txt. Recovered via
        // `git reset --hard` to the last known-good commit; never pushed. Clearing the same env
        // vars here is what makes this fixture safe to run under a hook, which is the whole
        // point of a test whose entire premise is "ambient GIT_DIR must not leak in".
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(decoy.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_CEILING_DIRECTORIES")
                .env_remove("GIT_NAMESPACE")
                .output()
                .expect("git must be installed to run this test")
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "co11b-decoy@example.invalid"]);
        run(&["config", "user.name", "co11b decoy"]);
        fs::write(decoy.path().join("decoy.txt"), "decoy").unwrap();
        run(&["add", "-A"]);
        let commit_out = run(&["commit", "--quiet", "-m", "decoy commit"]);
        assert!(
            commit_out.status.success(),
            "decoy repo commit must succeed: {commit_out:?}"
        );

        let non_git_dir = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_DIR", decoy.path().join(".git"));
        let commit = resolve_git_commit(non_git_dir.path());
        std::env::remove_var("GIT_DIR");
        assert_eq!(
            commit, "unknown",
            "MUTATION TARGET: an ambient GIT_DIR pointing at a REAL (decoy) repo must not leak that repo's HEAD into a non-git directory's resolved commit -- current_dir() alone does not override GIT_DIR, got: {commit}"
        );
    }

    #[test]
    fn resolve_git_commit_returns_real_sha_for_this_repo() {
        // Sanity check against the actual crate's own repo checkout, not a fixture.
        let repo_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let commit = resolve_git_commit(repo_dir);
        assert_ne!(commit, "unknown");
        assert_eq!(commit.len(), 40, "a real git sha is 40 hex chars: {commit}");
    }
}
