//! SP-1 acceptance tests: honest-seen gate via injected ClaimFileIdentity.
//!
//! Verifies that the split preserved the honest-seen contract at the GATE level:
//!
//! (a) TRUSTING: ClaimFileIdentity where local == live advances the bookmark (Ok).
//! (b) HONEST-SEEN: ClaimFileIdentity where local != live refuses (Err(NotLiveSession)),
//!     and the bookmark does NOT move (non-vacuous check).
//!
//! These are integration tests because they cross the crate boundary:
//! ClaimFileIdentity (fleet crate) + record_youyou_read (generic crate).

use buzz_seat_clerk_agencyos::claim_identity::ClaimFileIdentity;
use buzz_seat_listener::identity::SeatIdentity;
use buzz_seat_listener::read_state::{
    generate_slot_id, record_youyou_read, ReadGuardError, ReadStateWriter, SlotIdentity,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal claim file into `dir` for the given session_id and role.
/// Uses an RFC3339-like timestamp derived from the current wall clock so
/// freshness ordering is deterministic when only one claim is present.
fn write_claim_file(dir: &TempDir, role: &str, session_id: &str) {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Minimal RFC3339 UTC string (lexicographic ordering matches time ordering).
    let ts = format!("{now_secs}");
    let content = serde_json::json!({
        "session_id": session_id,
        "role": role,
        "cwd": "",
        "ts": ts,
    });
    let filename = format!("claude-seat-claim-{session_id}.json");
    std::fs::write(dir.path().join(filename), content.to_string()).expect("write claim file");
}

/// Build a minimal ReadStateWriter for tests (no disk I/O beyond the struct).
fn test_writer() -> ReadStateWriter {
    ReadStateWriter::new(SlotIdentity {
        slot_id: generate_slot_id(),
        client_id: generate_slot_id(),
    })
}

// ── Acceptance test (a): TRUSTING path ───────────────────────────────────────

/// SP-1 acceptance (a): when ClaimFileIdentity local == live, record_youyou_read
/// returns Ok(()) AND the bookmark advances from None to Some(ts).
///
/// Non-vacuous: we assert the bookmark moved, not just that Ok was returned.
#[test]
fn claim_identity_live_session_advances_bookmark() {
    let tmp = TempDir::new().expect("tempdir");

    let session_id = "live-session-abc-001";
    write_claim_file(&tmp, "foo-role", session_id);

    // ClaimFileIdentity for the same session that holds the claim.
    let identity =
        ClaimFileIdentity::new(session_id, Some("foo-role".to_string()), None, tmp.path());

    let local = identity.local_marker().expect("local_marker must be Some");
    let live = identity
        .live_marker()
        .expect("live_marker must resolve from claim file");

    // Sanity: they must be equal for the TRUSTING scenario.
    assert_eq!(
        local, live,
        "local and live markers must be equal when this IS the live session"
    );

    let mut writer = test_writer();
    let ctx = "chan-trusting-test".to_string();
    let ts = 1_720_000_001u64;

    // Before: bookmark absent.
    assert_eq!(
        writer.read_at_for(&ctx),
        None,
        "bookmark must be None before gate call"
    );

    let result = record_youyou_read(&mut writer, ctx.clone(), ts, &local, &live);

    assert!(
        result.is_ok(),
        "live session gate must return Ok; got: {:?}",
        result
    );

    // After: bookmark advanced (non-vacuous).
    assert_eq!(
        writer.read_at_for(&ctx),
        Some(ts),
        "bookmark must advance to {ts} when gate passes (live session)"
    );
}

// ── Acceptance test (b): HONEST-SEEN path ────────────────────────────────────

/// SP-1 acceptance (b): when ClaimFileIdentity local != live, record_youyou_read
/// returns Err(NotLiveSession) AND the bookmark stays at None.
///
/// Non-vacuous: we assert the bookmark did NOT move, not just that Err was returned.
#[test]
fn claim_identity_imposter_refuses_and_bookmark_stays_none() {
    let tmp = TempDir::new().expect("tempdir");

    let live_session = "live-session-real-001";
    let imposter_session = "imposter-session-xyz-999";

    // Claim dir holds a claim for the live session, NOT for the imposter.
    write_claim_file(&tmp, "foo-role", live_session);

    // ClaimFileIdentity constructed with the IMPOSTER session_id.
    let identity = ClaimFileIdentity::new(
        imposter_session,
        Some("foo-role".to_string()),
        None,
        tmp.path(),
    );

    let local = identity.local_marker().expect("local_marker must be Some");
    let live = identity
        .live_marker()
        .expect("live_marker must resolve from claim file");

    // Sanity: they must differ for the HONEST-SEEN scenario.
    assert_ne!(
        local, live,
        "imposter local marker must NOT equal live marker"
    );
    // Verify exact identity values via SessionMarker::new (the public constructor).
    use buzz_seat_listener::session_identity::SessionMarker;
    assert_eq!(
        local,
        SessionMarker::new(imposter_session),
        "local_marker must be the imposter session_id"
    );
    assert_eq!(
        live,
        SessionMarker::new(live_session),
        "live_marker must be the claim file's session_id"
    );

    let mut writer = test_writer();
    let ctx = "chan-honest-seen-test".to_string();
    let ts_attempt = 9_999_999_999u64;

    let result = record_youyou_read(&mut writer, ctx.clone(), ts_attempt, &local, &live);

    assert_eq!(
        result,
        Err(ReadGuardError::NotLiveSession),
        "imposter must be refused with Err(NotLiveSession); got: {:?}",
        result
    );

    // Non-vacuous: bookmark must NOT have moved.
    assert_eq!(
        writer.read_at_for(&ctx),
        None,
        "bookmark must remain None when gate refuses (imposter scenario)"
    );
}

// ── Acceptance test (c): bookmark contrast ───────────────────────────────────

/// Contrast test: same ctx, live session advances, then a second imposter call
/// on the same writer must NOT overwrite the bookmark.
///
/// This exercises the full round-trip: advance via live, then refuse via imposter,
/// and confirm the bookmark holds its live value.
#[test]
fn live_advances_then_imposter_cannot_overwrite() {
    let tmp = TempDir::new().expect("tempdir");

    let live_session = "live-session-contrast-001";
    let imposter_session = "imposter-session-contrast-999";
    write_claim_file(&tmp, "bar-role", live_session);

    let ctx = "chan-contrast-test".to_string();
    let ts_live: u64 = 1_720_000_100;
    let ts_imposter: u64 = 1_720_000_200;

    let mut writer = test_writer();

    // Step 1: live session advances the bookmark.
    let live_id =
        ClaimFileIdentity::new(live_session, Some("bar-role".to_string()), None, tmp.path());
    let local_live = live_id.local_marker().unwrap();
    let live_marker = live_id.live_marker().unwrap();

    record_youyou_read(&mut writer, ctx.clone(), ts_live, &local_live, &live_marker)
        .expect("live session must advance bookmark");
    assert_eq!(writer.read_at_for(&ctx), Some(ts_live));

    // Step 2: imposter tries to overwrite with a higher timestamp.
    let imposter_id = ClaimFileIdentity::new(
        imposter_session,
        Some("bar-role".to_string()),
        None,
        tmp.path(),
    );
    let local_imposter = imposter_id.local_marker().unwrap();
    let live_via_imposter = imposter_id.live_marker().unwrap();

    let result = record_youyou_read(
        &mut writer,
        ctx.clone(),
        ts_imposter,
        &local_imposter,
        &live_via_imposter,
    );

    assert_eq!(
        result,
        Err(ReadGuardError::NotLiveSession),
        "imposter must be refused even with a higher timestamp"
    );

    // Bookmark must still be ts_live, not ts_imposter.
    assert_eq!(
        writer.read_at_for(&ctx),
        Some(ts_live),
        "bookmark must stay at ts_live after imposter refusal"
    );
}
