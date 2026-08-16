//! Fleet-specific SeatIdentity implementation.
//!
//! `ClaimFileIdentity` resolves the live session marker by reading
//! `claude-seat-claim-*.json` files from a configured directory (defaults to
//! `/tmp`). The file with the lexicographically greatest `ts` field (RFC3339
//! UTC) whose `role` matches wins. This is the honest-seen behavior: the gate
//! in `record_youyou_read` passes only when the reader IS the live session.
//!
//! The local marker is the `session_id` of the running clerk, passed at
//! construction. If local == live, this IS the live session; otherwise the
//! gate returns `Err(ReadGuardError::NotLiveSession)`.

use std::path::PathBuf;

use buzz_seat_listener::identity::SeatIdentity;
use buzz_seat_listener::session_identity::SessionMarker;
use serde::Deserialize;
use tracing::{debug, warn};

// ── Claim-file JSON shape ─────────────────────────────────────────────────────

/// JSON shape of `/tmp/claude-seat-claim-<sid>.json` files written by the
/// fleet harness to advertise which session holds a given seat role.
#[derive(Debug, Deserialize)]
struct SeatClaimFile {
    session_id: String,
    role: String,
    /// Optional cwd for disambiguation when multiple seats share a role.
    #[serde(default)]
    cwd: String,
    /// RFC3339 UTC timestamp string. Lexicographic max picks the freshest.
    ts: String,
}

// ── ClaimFileIdentity ─────────────────────────────────────────────────────────

/// Fleet [`SeatIdentity`] implementation that resolves liveness from claim files.
///
/// Construct with the `session_id` of the running clerk, the seat role used
/// to filter claim files, an optional cwd for further disambiguation, and the
/// directory where claim files live (typically `/tmp`).
pub struct ClaimFileIdentity {
    /// session_id of THIS running clerk instance.
    session_id: String,
    /// Seat role used to filter claim files (e.g. "agencyos-cc").
    role: Option<String>,
    /// Optional cwd for disambiguation when multiple seats share a role.
    cwd: Option<String>,
    /// Directory to scan for `claude-seat-claim-*.json` files.
    claim_dir: PathBuf,
}

impl ClaimFileIdentity {
    /// Create a new `ClaimFileIdentity`.
    ///
    /// - `session_id`: the unique id of this running clerk process.
    /// - `role`: optional role string used to filter which claim files to read.
    /// - `cwd`: optional working directory for further disambiguation.
    /// - `claim_dir`: directory to scan for claim files (typically `/tmp`).
    pub fn new(
        session_id: impl Into<String>,
        role: Option<String>,
        cwd: Option<String>,
        claim_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            role,
            cwd,
            claim_dir: claim_dir.into(),
        }
    }

    /// Resolve the live session marker from claim files in `self.claim_dir`.
    ///
    /// Scans all files matching `claude-seat-claim-*.json`, filters by role
    /// (and optionally cwd), and returns the `session_id` wrapped in a
    /// `SessionMarker` for the file with the highest `ts` value. Unreadable
    /// or malformed files are silently skipped. Returns `None` if no valid
    /// matching claim is found.
    pub(crate) fn resolve_live_marker_from_claims(&self) -> Option<SessionMarker> {
        let dir = &self.claim_dir;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                warn!("claim_identity: cannot read claim_dir {:?}: {}", dir, err);
                return None;
            }
        };

        // best: (ts_string, session_id)
        let mut best: Option<(String, String)> = None;

        for entry in entries.flatten() {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if !fname_str.starts_with("claude-seat-claim-") || !fname_str.ends_with(".json") {
                continue;
            }

            let path = entry.path();
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(err) => {
                    warn!("claim_identity: cannot read {:?}: {}", path, err);
                    continue;
                }
            };

            let claim: SeatClaimFile = match serde_json::from_str(&raw) {
                Ok(c) => c,
                Err(err) => {
                    warn!("claim_identity: cannot parse {:?}: {}", path, err);
                    continue;
                }
            };

            // Filter by role if configured.
            if let Some(ref expected_role) = self.role {
                if claim.role != *expected_role {
                    debug!(
                        "claim_identity: skipping {:?} (role {:?} != {:?})",
                        path, claim.role, expected_role
                    );
                    continue;
                }
            }

            // Filter by cwd if configured.
            // SECURITY: honest-seen is a security gate. A claim whose cwd does
            // not exactly match a seat's configured cwd must be rejected,
            // including an empty cwd. This prevents a stale or malformed claim
            // from matching every seat that shares a role.
            if let Some(ref expected_cwd) = self.cwd {
                if claim.cwd != *expected_cwd {
                    debug!(
                        "claim_identity: skipping {:?} (cwd {:?} != {:?})",
                        path, claim.cwd, expected_cwd
                    );
                    continue;
                }
            }

            let sid = claim.session_id.trim().to_string();
            if sid.is_empty() {
                continue;
            }

            let is_better = match &best {
                None => true,
                Some((best_ts, _)) => claim.ts > *best_ts,
            };
            if is_better {
                best = Some((claim.ts, sid));
            }
        }

        best.map(|(_, sid)| SessionMarker::new(sid))
    }
}

impl SeatIdentity for ClaimFileIdentity {
    /// Returns the marker of THIS running clerk instance.
    ///
    /// This is the identity the clerk stamps on its own reads. If this
    /// matches `live_marker()`, the gate in `record_youyou_read` passes.
    /// Returns `None` only if the session_id is empty (safe-fail: gate skips
    /// bookmark advance when local identity is unknown).
    fn local_marker(&self) -> Option<SessionMarker> {
        if self.session_id.trim().is_empty() {
            None
        } else {
            Some(SessionMarker::new(self.session_id.clone()))
        }
    }

    /// Returns the marker of the currently live session, resolved from claim files.
    ///
    /// Returns `None` if no valid matching claim file is found, which causes
    /// the gate to skip bookmark advance (safe-fail).
    fn live_marker(&self) -> Option<SessionMarker> {
        self.resolve_live_marker_from_claims()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn write_claim(dir: &std::path::Path, session_id: &str, role: &str, cwd: &str, ts: &str) {
        let filename = format!("claude-seat-claim-{session_id}.json");
        let content = serde_json::json!({
            "session_id": session_id,
            "role": role,
            "cwd": cwd,
            "ts": ts,
        })
        .to_string();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    fn make_identity(
        session_id: &str,
        role: Option<&str>,
        claim_dir: &std::path::Path,
    ) -> ClaimFileIdentity {
        ClaimFileIdentity::new(session_id, role.map(str::to_string), None, claim_dir)
    }

    // ── local_marker tests ────────────────────────────────────────────────────

    #[test]
    fn local_marker_returns_configured_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let id = ClaimFileIdentity::new("my-session-abc", None, None, dir.path());
        let marker = id.local_marker().expect("local_marker should be Some");
        assert_eq!(marker, SessionMarker::new("my-session-abc".to_string()));
    }

    #[test]
    fn local_marker_returns_none_for_empty_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let id = ClaimFileIdentity::new("", None, None, dir.path());
        assert!(
            id.local_marker().is_none(),
            "empty session_id should yield None"
        );
    }

    // ── live_marker / resolve_live_marker_from_claims tests ──────────────────

    #[test]
    fn live_marker_freshest_ts_wins() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        write_claim(tmp, "sid-old", "MyRole", "/cwd", "2026-08-01T00:00:00Z");
        write_claim(tmp, "sid-new", "MyRole", "/cwd", "2026-08-14T00:55:55Z");

        let id = make_identity("any-session", Some("MyRole"), tmp);
        let marker = id.live_marker().expect("should resolve freshest claim");
        assert_eq!(marker, SessionMarker::new("sid-new".to_string()));
    }

    #[test]
    fn live_marker_non_matching_role_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        write_claim(tmp, "sid-a", "WrongRole", "/cwd", "2026-08-14T00:00:00Z");

        let id = make_identity("any-session", Some("MyRole"), tmp);
        assert!(
            id.live_marker().is_none(),
            "non-matching role should yield None"
        );
    }

    #[test]
    fn live_marker_no_role_filter_picks_newest_across_roles() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        write_claim(tmp, "sid-alpha", "RoleA", "/cwd", "2026-08-14T01:00:00Z");
        write_claim(tmp, "sid-beta", "RoleB", "/cwd", "2026-08-14T02:00:00Z");

        // No role filter: should pick whichever has newer ts.
        let id = ClaimFileIdentity::new("any-session", None, None, tmp);
        let marker = id
            .live_marker()
            .expect("should resolve without role filter");
        assert_eq!(marker, SessionMarker::new("sid-beta".to_string()));
    }

    #[test]
    fn live_marker_cwd_disambiguation() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        write_claim(
            tmp,
            "sid-alpha",
            "SharedRole",
            "/cwd/alpha",
            "2026-08-14T01:00:00Z",
        );
        write_claim(
            tmp,
            "sid-beta",
            "SharedRole",
            "/cwd/beta",
            "2026-08-14T02:00:00Z",
        );

        // With cwd filter for alpha: should get sid-alpha even though sid-beta is newer.
        let id_alpha = ClaimFileIdentity::new(
            "s",
            Some("SharedRole".into()),
            Some("/cwd/alpha".into()),
            tmp,
        );
        let marker = id_alpha.live_marker().expect("should find alpha");
        assert_eq!(marker, SessionMarker::new("sid-alpha".to_string()));

        // Without cwd filter: should get sid-beta (newest ts).
        let id_any = ClaimFileIdentity::new("s", Some("SharedRole".into()), None, tmp);
        let marker = id_any.live_marker().expect("should find beta");
        assert_eq!(marker, SessionMarker::new("sid-beta".to_string()));
    }

    #[test]
    fn live_marker_empty_dir_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let id = make_identity("any-session", Some("AnyRole"), dir.path());
        assert!(
            id.live_marker().is_none(),
            "empty claim dir should yield None"
        );
    }

    #[test]
    fn live_marker_malformed_file_skipped_valid_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        // Write one malformed file.
        std::fs::write(tmp.join("claude-seat-claim-bad.json"), "not valid json {{{").unwrap();

        // Write one valid file.
        write_claim(tmp, "sid-good", "GoodRole", "/cwd", "2026-08-14T00:00:00Z");

        let id = make_identity("any-session", Some("GoodRole"), tmp);
        let marker = id.live_marker().expect("valid file should resolve");
        assert_eq!(marker, SessionMarker::new("sid-good".to_string()));
    }

    #[test]
    fn live_marker_unrelated_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        // Files that do NOT match the naming prefix/suffix.
        std::fs::write(
            tmp.join("some-other-file.json"),
            r#"{"session_id":"x","role":"R","cwd":"","ts":"2026-08-14T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(tmp.join("claude-seat-claim-noext"), "anything").unwrap();

        let id = make_identity("any-session", Some("R"), tmp);
        assert!(
            id.live_marker().is_none(),
            "files with wrong name pattern must be ignored"
        );
    }

    // ── Honest-seen gate integration ──────────────────────────────────────────

    #[test]
    fn live_session_matches_local_passes_gate() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        let my_session = "live-clerk-session-xyz";
        write_claim(
            tmp,
            my_session,
            "agencyos-cc",
            "/home/seat",
            "2026-08-14T10:00:00Z",
        );

        let id = ClaimFileIdentity::new(my_session, Some("agencyos-cc".into()), None, tmp);

        let local = id.local_marker().expect("local should be Some");
        let live = id.live_marker().expect("live should resolve");
        assert_eq!(local, live, "when this IS the live session, local == live");
    }

    #[test]
    fn live_marker_empty_cwd_claim_rejected_when_filter_set() {
        // When the identity has a non-empty cwd filter, a claim file whose cwd
        // field is "" must be rejected. An empty cwd is not a wildcard; it is a
        // malformed or stale claim and the honest-seen gate must not let it pass.
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        // Write a claim with an empty cwd string, matching role.
        write_claim(tmp, "sid-empty-cwd", "TestRole", "", "2026-08-16T00:00:00Z");

        let id = ClaimFileIdentity::new(
            "any-session",
            Some("TestRole".into()),
            Some("/expected/cwd".into()),
            tmp,
        );
        assert!(
            id.live_marker().is_none(),
            "empty-cwd claim must be rejected when cwd filter is set"
        );
    }

    #[test]
    fn imposter_session_does_not_match_live() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        let live_session = "real-live-clerk-session";
        let my_session = "imposter-session-id";

        // The live claim belongs to another session.
        write_claim(
            tmp,
            live_session,
            "agencyos-cc",
            "/home/seat",
            "2026-08-14T10:00:00Z",
        );

        let id = ClaimFileIdentity::new(my_session, Some("agencyos-cc".into()), None, tmp);

        let local = id.local_marker().expect("local should be Some");
        let live = id.live_marker().expect("live should resolve");
        assert_ne!(
            local, live,
            "imposter local marker must not equal live marker"
        );
    }
}
