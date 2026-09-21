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

test("CL-N4 (MUTATION TARGET): clerk_alias_from_plist_filename reverses clerk_plist_filename for any alias", () => {
  for (const alias of ["holdout", "am", "creativedire"]) {
    const filename = bash(`clerk_plist_filename "${alias}"`).stdout.trim();
    const roundTrip = bash(
      `clerk_alias_from_plist_filename "${filename}"`,
    ).stdout.trim();
    assert.equal(
      roundTrip,
      alias,
      `MUTATION TARGET: round-tripping ${filename} must recover "${alias}"`,
    );
  }
});

test("CL-N5: clerk_run_pid_path is a deterministic function of run_dir and alias", () => {
  assert.equal(
    bash('clerk_run_pid_path "/tmp/run" "holdout"').stdout.trim(),
    "/tmp/run/clerk-holdout.pid",
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
    'render_clerk_wrapper_script "/bin/clerk" "overwatch" "OVERWATCH_NSEC" "wss://relay" "AgencyOS-Overwatch" "sess-1" "/tmp/wake.json" "/tmp/readack.json" "/tmp" "/tmp" "/tmp/keys"',
  ).stdout;
  assert.match(
    script,
    /export SEAT_NSEC="\$\{OVERWATCH_NSEC\}"/,
    "MUTATION TARGET: must reference the KEYVAR by name, deferred to source-time -- never bake in a value",
  );
  assert.match(
    script,
    /\. "\$KEY_FILE"/,
    "must source the per-seat header file (via $KEY_FILE) before exporting SEAT_NSEC",
  );
  assert.match(
    script,
    /KEY_FILE="\/tmp\/keys\/overwatch\.env"/,
    "must point KEY_FILE at keys_dir\\/<alias>.env",
  );
  assert.match(
    script,
    /exec "\/bin\/clerk"/,
    "must exec the canonical clerk binary, replacing the wrapper process (no supervisor leak)",
  );
});

test("CL-W1b (MUTATION TARGET): the wrapper never sources anything under Documents", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "overwatch" "OVERWATCH_NSEC" "wss://relay" "AgencyOS-Overwatch" "sess-1" "/tmp/wake.json" "/tmp/readack.json" "/tmp" "/tmp"',
  ).stdout;
  assert.doesNotMatch(
    script,
    /Documents/,
    "MUTATION TARGET: opeff#1227 -- a launchd-started process is refused under Documents on this host; the wrapper must never reference that path",
  );
});

test("CL-W1c (MUTATION TARGET): the wrapper refuses loud, naming place-clerk-keys.sh, when the header file is missing", () => {
  const dir = "/tmp/clerk-lib-test-missing";
  spawnSync("rm", ["-rf", dir]);
  const script = bash(
    `render_clerk_wrapper_script "/bin/clerk" "ghost" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp" "${dir}"`,
  ).stdout;
  const run = spawnSync("bash", ["-c", script], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
  });
  assert.notEqual(run.status, 0, "must exit non-zero when the header file is missing");
  assert.match(
    run.stderr,
    /place-clerk-keys\.sh/,
    `must name the placement script in the refusal, got:\n${run.stderr}`,
  );
});

test("CL-W1d (MUTATION TARGET): the wrapper refuses loud, naming place-clerk-keys.sh, when the header file is not mode 600", () => {
  const dir = "/tmp/clerk-lib-test-badmode";
  spawnSync("rm", ["-rf", dir]);
  spawnSync("mkdir", ["-p", dir]);
  spawnSync("bash", ["-c", `printf 'K=x\\n' > ${dir}/ghost.env`]);
  spawnSync("chmod", ["644", `${dir}/ghost.env`]);
  const script = bash(
    `render_clerk_wrapper_script "/bin/clerk" "ghost" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp" "${dir}"`,
  ).stdout;
  const run = spawnSync("bash", ["-c", script], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
  });
  assert.notEqual(run.status, 0, "must exit non-zero when the header file is mode 644");
  assert.match(
    run.stderr,
    /place-clerk-keys\.sh/,
    `must name the placement script in the refusal, got:\n${run.stderr}`,
  );
});

test("CL-W2: render_clerk_wrapper_script sets every env var the clerk binary reads", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "ops" "K" "wss://relay" "AgencyOS-Ops" "sess-2" "/tmp/wake.json" "/tmp/readack.json" "/tmp/claims" "/tmp/logs"',
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
    'render_clerk_wrapper_script "/bin/clerk" "seat" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp"',
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


// ── pid file, opeff#1210 ─────────────────────────────────────────────────────────────────────
// CLERKALIVE01, CLERKBIN01 and CLERKSTRAY01 read run/clerk-<alias>.pid. A launchd clerk that
// writes none reads as dead and as a stray at once. The wrapper writes its own pid before exec,
// under $HOME, never under Documents.

test("CL-W5 (MUTATION TARGET): the rendered wrapper writes clerk-<alias>.pid into the run dir before exec, never under Documents", () => {
  const script = bash(
    'render_clerk_wrapper_script "/bin/clerk" "ops" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp" "/tmp/keys" "/tmp/runs"',
  ).stdout;
  assert.match(script, /PID_DIR="\/tmp\/runs"/, "must take the run dir from the twelfth argument");
  assert.match(script, /echo "\$\$" > "\$PID_DIR\/clerk-ops\.pid"/, "MUTATION TARGET: must write its own pid to clerk-<alias>.pid");
  const writeAt = script.indexOf('clerk-ops.pid');
  const execAt = script.indexOf('exec "/bin/clerk"');
  assert.ok(writeAt > 0 && execAt > writeAt, "the pid write must come before the exec");
  const defaultScript = bash(
    'render_clerk_wrapper_script "/bin/clerk" "ops" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp"',
  ).stdout;
  assert.match(defaultScript, /PID_DIR="\$HOME\/\.local\/agencyos\/run"/, "default run dir is under $HOME");
  assert.doesNotMatch(defaultScript, /Documents/, "no Documents path anywhere in the wrapper");
});

test("CL-W5b: executed under a scratch HOME with a fixture clerk, the wrapper leaves clerk-<alias>.pid holding the pid the clerk ran as", () => {
  const dir = "/tmp/clerk-lib-test-pid";
  spawnSync("rm", ["-rf", dir]);
  spawnSync("mkdir", ["-p", `${dir}/keys`, `${dir}/bin`]);
  spawnSync("bash", ["-c", `printf 'K=x\\n' > ${dir}/keys/ghost.env && chmod 600 ${dir}/keys/ghost.env`]);
  // The fixture clerk records the pid it runs as, so the test can compare it to the pid file.
  spawnSync("bash", ["-c", `printf '#!/bin/sh\\necho $$ > ${dir}/ran-as.txt\\n' > ${dir}/bin/clerk && chmod +x ${dir}/bin/clerk`]);
  const script = bash(
    `render_clerk_wrapper_script "${dir}/bin/clerk" "ghost" "K" "wss://relay" "role" "sess" "/tmp/w" "/tmp/r" "/tmp" "/tmp" "${dir}/keys" "${dir}/run"`,
  ).stdout;
  const run = spawnSync("bash", ["-c", script], { encoding: "utf8", timeout: HARD_TIMEOUT_MS });
  assert.equal(run.status, 0, `wrapper must exit 0 with a good header, got:\n${run.stderr}`);
  const pidFile = spawnSync("cat", [`${dir}/run/clerk-ghost.pid`], { encoding: "utf8" }).stdout.trim();
  const ranAs = spawnSync("cat", [`${dir}/ran-as.txt`], { encoding: "utf8" }).stdout.trim();
  assert.match(pidFile, /^\d+$/, "pid file must hold one number");
  assert.equal(pidFile, ranAs, "the pid file must name the pid the clerk itself ran as, since exec keeps the pid");
});

// ── named launcher, opeff#1232 ───────────────────────────────────────────────────────────────
test("CL-L1: clerk_launcher_name drops spaces and a leading AgencyOS- so Login Items reads AgencyOS-Clerk-<Role>", () => {
  for (const [role, want] of [["Art Director", "AgencyOS-Clerk-ArtDirector"], ["Ops", "AgencyOS-Clerk-Ops"], ["AgencyOS-Overwatch", "AgencyOS-Clerk-Overwatch"], ["Sub-TP", "AgencyOS-Clerk-Sub-TP"]]) {
    assert.equal(bash(`clerk_launcher_name "${role}"`).stdout.trim(), want);
  }
});

test("CL-L2 (MUTATION TARGET): render_clerk_launcher_source execs the wrapper through bash and names the launcher in its header", () => {
  const src = bash('render_clerk_launcher_source "AgencyOS-Clerk-Ops" "/tmp/x/run-clerk-ops.sh"').stdout;
  assert.match(src, /execl\("\/bin\/bash", "\/bin\/bash", "\/tmp\/x\/run-clerk-ops\.sh", \(char \*\)0\)/);
  assert.match(src, /AgencyOS-Clerk-Ops/);
  assert.match(src, /GENERATED/);
});
