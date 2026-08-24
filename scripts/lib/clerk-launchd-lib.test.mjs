/**
 * scripts/lib/clerk-launchd-lib.test.mjs
 *
 * comms-orch#18: pure-function tests for the launchd plist/wrapper generator, mirroring
 * agencyos-terminal-driver's relaunch-lib.test.js convention (source the lib alone in `bash -c`,
 * no registry/filesystem/launchctl side effects). See scripts/clerk-launchd-daemon.test.mjs for
 * the real-process, real-launchd acceptance test.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const LIB = join(HERE, "clerk-launchd-lib.sh");
const HARD_TIMEOUT_MS = 10_000;

function bash(script) {
  return spawnSync("bash", ["-c", `source "${LIB}"; ${script}`], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
  });
}

// ── naming ───────────────────────────────────────────────────────────────────────────────────

test("CL-N1: clerk_plist_label is per-seat, never a shared label across aliases", () => {
  const a = bash('clerk_plist_label "AgencyOS-Overwatch"').stdout.trim();
  const b = bash('clerk_plist_label "AgencyOS-Ops"').stdout.trim();
  assert.equal(a, "com.civilization.buzz-seat-clerk-AgencyOS-Overwatch");
  assert.equal(b, "com.civilization.buzz-seat-clerk-AgencyOS-Ops");
  assert.notEqual(
    a,
    b,
    "MUTATION TARGET: two different seats must never collide on one label",
  );
});

test("CL-N2: clerk_plist_filename and clerk_wrapper_filename are discoverable, deterministic functions of the alias", () => {
  assert.equal(
    bash('clerk_plist_filename "holdout"').stdout.trim(),
    "com.civilization.buzz-seat-clerk-holdout.plist",
  );
  assert.equal(
    bash('clerk_wrapper_filename "holdout"').stdout.trim(),
    "run-clerk-holdout.sh",
  );
});

test("CL-N3 (MUTATION TARGET): clerk_launchd_stdout_path and clerk_launchd_stderr_path are distinct paths, both keyed by alias", () => {
  const out = bash('clerk_launchd_stdout_path "/tmp" "ops"').stdout.trim();
  const err = bash('clerk_launchd_stderr_path "/tmp" "ops"').stdout.trim();
  assert.equal(out, "/tmp/buzz-seat-clerk-ops.launchd-stdout.log");
  assert.equal(err, "/tmp/buzz-seat-clerk-ops.launchd-stderr.log");
  assert.notEqual(
    out,
    err,
    "MUTATION TARGET: stdout and stderr must never resolve to the same file",
  );
});

// ── wrapper script rendering ─────────────────────────────────────────────────────────────────

test("CL-W1 (MUTATION TARGET): render_clerk_wrapper_script embeds the KEYVAR NAME, never a literal secret value", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "/tmp/env.local" "OVERWATCH_NSEC" "wss://relay" "AgencyOS-Overwatch" "sess-1" "/tmp/wake.json" "/tmp/readack.json" "/tmp" "/tmp"',
  ).stdout;
  assert.match(
    script,
    /export SEAT_NSEC="\$\{OVERWATCH_NSEC\}"/,
    "MUTATION TARGET: must reference the KEYVAR by name, deferred to source-time -- never bake in a value",
  );
  assert.match(
    script,
    /\. "\/tmp\/env\.local"/,
    "must source ENVLOCAL before exporting SEAT_NSEC",
  );
  assert.match(
    script,
    /exec "\/bin\/clerk"/,
    "must exec the canonical clerk binary, replacing the wrapper process (no supervisor leak)",
  );
});

test("CL-W2: render_clerk_wrapper_script sets every env var the clerk binary reads", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "/tmp/env.local" "K" "wss://relay" "AgencyOS-Ops" "sess-2" "/tmp/wake.json" "/tmp/readack.json" "/tmp/claims" "/tmp/logs"',
  ).stdout;
  for (const [key, value] of [
    ["RELAY_URL", "wss://relay"],
    ["SEAT_ROLE", "AgencyOS-Ops"],
    ["SEAT_SESSION", "sess-2"],
    ["WAKE_FILE", "/tmp/wake.json"],
    ["READACK_FILE", "/tmp/readack.json"],
    ["CLAIM_DIR", "/tmp/claims"],
    ["CLERK_LOG_DIR", "/tmp/logs"],
  ]) {
    assert.match(
      script,
      new RegExp(`export ${key}="${value.replace(/\//g, "\\/")}"`),
      `missing or wrong ${key}: ${script}`,
    );
  }
});

test("CL-W3: the wrapper is a valid bash script (set -euo pipefail, no syntax errors)", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "/tmp/env.local" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp"',
  ).stdout;
  const check = spawnSync("bash", ["-n", "/dev/stdin"], {
    input: script,
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
  });
  assert.equal(
    check.status,
    0,
    `generated wrapper has a syntax error: ${check.stderr}`,
  );
});

// ── plist rendering ──────────────────────────────────────────────────────────────────────────

test("CL-P1 (MUTATION TARGET): render_clerk_plist produces a valid, parseable plist with every required key", () => {
  const r = bash(
    'render_clerk_plist "com.civilization.buzz-seat-clerk-test" "/tmp/run-clerk-test.sh" "/tmp/out.log" "/tmp/err.log"',
  );
  assert.equal(r.status, 0, r.stderr);
  const parseCheck = spawnSync(
    "python3",
    [
      "-c",
      `
import plistlib, sys
d = plistlib.loads(sys.stdin.buffer.read())
assert d['Label'] == 'com.civilization.buzz-seat-clerk-test', d
assert d['ProgramArguments'] == ['/tmp/run-clerk-test.sh'], d
assert d['RunAtLoad'] is True, d
assert d['KeepAlive'] is True, d
assert d['StandardOutPath'] == '/tmp/out.log', d
assert d['StandardErrorPath'] == '/tmp/err.log', d
print('OK')
`,
    ],
    { input: r.stdout, encoding: "utf8", timeout: HARD_TIMEOUT_MS },
  );
  assert.equal(
    parseCheck.stdout.trim(),
    "OK",
    `MUTATION TARGET: plist missing/wrong required key(s): ${parseCheck.stderr}`,
  );
});

test("CL-P2: render_clerk_plist safely escapes a path containing XML-special characters", () => {
  const r = bash(
    `render_clerk_plist "com.civilization.buzz-seat-clerk-test" "/tmp/run & <clerk>.sh" "/tmp/out.log" "/tmp/err.log"`,
  );
  assert.equal(r.status, 0, r.stderr);
  const parseCheck = spawnSync(
    "python3",
    [
      "-c",
      `
import plistlib, sys
d = plistlib.loads(sys.stdin.buffer.read())
assert d['ProgramArguments'] == ['/tmp/run & <clerk>.sh'], d
print('OK')
`,
    ],
    { input: r.stdout, encoding: "utf8", timeout: HARD_TIMEOUT_MS },
  );
  assert.equal(
    parseCheck.stdout.trim(),
    "OK",
    `special characters must round-trip through the real XML escaper: ${parseCheck.stderr}`,
  );
});
