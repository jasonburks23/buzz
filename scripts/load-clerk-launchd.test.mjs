/**
 * scripts/load-clerk-launchd.test.mjs
 *
 * opeff#1210: wiring test for load-clerk-launchd.sh, the missing "load" step
 * generate-clerk-launchd.sh (comms-orch#18) left to operator hands. Never touches the real
 * fleet registry, the real installed clerk binary, the real ~/Library/LaunchAgents, or the real
 * launchctl -- HOME and LAUNCHCTL_BIN are always redirected to scratch fixtures.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  writeFileSync,
  mkdirSync,
  chmodSync,
  readFileSync,
  readdirSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCRIPT = join(HERE, "load-clerk-launchd.sh");
const HARD_TIMEOUT_MS = 15_000;

function renderPlist(label, programPath) {
  const r = spawnSync(
    "python3",
    [
      "-c",
      `
import plistlib, sys
label, program = sys.argv[1:3]
doc = {"Label": label, "ProgramArguments": [program], "RunAtLoad": True, "KeepAlive": True}
sys.stdout.buffer.write(plistlib.dumps(doc, fmt=plistlib.FMT_XML))
`,
      label,
      programPath,
    ],
    { encoding: "utf8", timeout: HARD_TIMEOUT_MS },
  );
  if (r.status !== 0) throw new Error(`plist render failed: ${r.stderr}`);
  return r.stdout;
}

function makeFixture() {
  const dir = mkdtempSync(join(tmpdir(), "lcl-fixture-"));

  const deployDir = join(dir, "deploy");
  mkdirSync(deployDir, { recursive: true });
  writeFileSync(
    join(deployDir, "com.civilization.buzz-seat-clerk-seatone.plist"),
    renderPlist(
      "com.civilization.buzz-seat-clerk-seatone",
      join(deployDir, "run-clerk-seatone.sh"),
    ),
  );
  writeFileSync(join(deployDir, "run-clerk-seatone.sh"), "#!/bin/sh\nexit 0\n");
  writeFileSync(
    join(deployDir, "com.civilization.buzz-seat-clerk-seattwo.plist"),
    renderPlist(
      "com.civilization.buzz-seat-clerk-seattwo",
      join(deployDir, "run-clerk-seattwo.sh"),
    ),
  );
  writeFileSync(join(deployDir, "run-clerk-seattwo.sh"), "#!/bin/sh\nexit 0\n");

  const runDir = join(dir, "run");
  mkdirSync(runDir, { recursive: true });

  const scratchHome = join(dir, "home");
  mkdirSync(scratchHome, { recursive: true });

  const stubDir = mkdtempSync(join(tmpdir(), "lcl-launchctl-stub-"));
  const callLog = join(stubDir, "calls.log");
  writeFileSync(
    join(stubDir, "launchctl"),
    `#!/bin/sh\necho "$@" >> "${callLog}"\nif [ "$1" = "list" ]; then echo "1234\t0\t$2"; fi\n`,
  );
  chmodSync(join(stubDir, "launchctl"), 0o755);

  return { dir, deployDir, runDir, scratchHome, stubDir, callLog };
}

function run(fixture, args) {
  return spawnSync("bash", [SCRIPT, ...args], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      HOME: fixture.scratchHome,
      CLERK_LAUNCHD_DEPLOY_DIR: fixture.deployDir,
      CLERK_RUN_DIR: fixture.runDir,
      LAUNCHCTL_BIN: join(fixture.stubDir, "launchctl"),
    },
  });
}

test("LCL-1: dry run (no args) lists every seat found in DEPLOY_DIR, its plist path, and its bare-pid status", () => {
  const f = makeFixture();
  const r = run(f, []);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);
  assert.match(r.stdout, /seatone/);
  assert.match(r.stdout, /seattwo/);
  assert.match(
    r.stdout,
    /com\.civilization\.buzz-seat-clerk-seatone\.plist/,
    r.stdout,
  );
  assert.match(
    r.stdout,
    /no (bare )?pid file/i,
    `expected a named "no pid file" status for a seat with nothing in CLERK_RUN_DIR, got:\n${r.stdout}`,
  );
});

test("LCL-2: dry run writes nothing under scratch HOME and never invokes launchctl", () => {
  const f = makeFixture();
  const r = run(f, []);
  assert.equal(r.status, 0, r.stderr);
  assert.ok(
    !existsSync(join(f.scratchHome, "Library", "LaunchAgents")),
    "MUTATION TARGET: dry run must not create ~/Library/LaunchAgents",
  );
  assert.ok(
    !existsSync(f.callLog),
    `MUTATION TARGET: dry run must never invoke launchctl. Calls seen: ${
      existsSync(f.callLog) ? readFileSync(f.callLog, "utf8") : ""
    }`,
  );
});

test("LCL-3 (MUTATION TARGET): --live stops the bare pid by literal number before bootstrapping, so two clerks never run for one seat", () => {
  const f = makeFixture();

  // The bare clerk and the loader must run as descendants of the SAME shell invocation: signal
  // delivery between two independently-spawned process trees is not reliable in this sandboxed
  // harness (proved separately -- kill -TERM from one spawnSync tree to a pid owned by another
  // silently no-ops here), which real launchd hosts do not do. Backgrounding the throwaway sleep
  // inside the same `bash -c` as the loader call keeps both in one tree, exactly like the real
  // tab-clerk-<alias>.sh background job and the loader running on the same host.
  const pidFile = join(f.runDir, "clerk-seatone.pid");
  const script = [
    "set -e",
    "sleep 1000 &",
    "barepid=$!",
    `echo "$barepid" > ${JSON.stringify(pidFile)}`,
    `bash ${JSON.stringify(SCRIPT)} --live seatone`,
    "status=$?",
    'if kill -0 "$barepid" 2>/dev/null; then echo BARE_STILL_ALIVE; kill -KILL "$barepid" 2>/dev/null || true; else echo BARE_DEAD; fi',
    "exit $status",
  ].join("\n");

  const r = spawnSync("bash", ["-c", script], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      HOME: f.scratchHome,
      CLERK_LAUNCHD_DEPLOY_DIR: f.deployDir,
      CLERK_RUN_DIR: f.runDir,
      LAUNCHCTL_BIN: join(f.stubDir, "launchctl"),
    },
  });
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);

  // MUTATION TARGET: a mutation that skips the stop leaves BARE_STILL_ALIVE in the output.
  assert.match(
    r.stdout,
    /BARE_DEAD/,
    `MUTATION TARGET: the bare background clerk pid must be stopped before --live loads the plist, got:\n${r.stdout}`,
  );
  assert.doesNotMatch(r.stdout, /BARE_STILL_ALIVE/, r.stdout);

  const destPlist = join(
    f.scratchHome,
    "Library",
    "LaunchAgents",
    "com.civilization.buzz-seat-clerk-seatone.plist",
  );
  assert.ok(existsSync(destPlist), `expected plist copied to ${destPlist}`);

  const calls = readFileSync(f.callLog, "utf8");
  assert.match(
    calls,
    /bootstrap gui\/\d+ .*com\.civilization\.buzz-seat-clerk-seatone\.plist/,
    `expected a bootstrap call for seatone, got:\n${calls}`,
  );
  assert.match(
    calls,
    /list com\.civilization\.buzz-seat-clerk-seatone/,
    `expected the script to report launchctl list state, got:\n${calls}`,
  );
  assert.match(
    r.stdout,
    /1234/,
    `expected the printed launchctl list state to reach stdout, got:\n${r.stdout}`,
  );
});

test("LCL-4: --live on a seat with no bare pid file skips the stop by name, without failing", () => {
  const f = makeFixture();
  const r = run(f, ["--live", "seattwo"]);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);
  assert.match(
    r.stdout,
    /seattwo.*no bare clerk/i,
    `expected a named "nothing to stop" message, got:\n${r.stdout}`,
  );
  assert.ok(
    existsSync(
      join(
        f.scratchHome,
        "Library",
        "LaunchAgents",
        "com.civilization.buzz-seat-clerk-seattwo.plist",
      ),
    ),
  );
});

test("LCL-5: --all processes every discovered seat", () => {
  const f = makeFixture();
  const r = run(f, ["--live", "--all"]);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);
  for (const alias of ["seatone", "seattwo"]) {
    assert.ok(
      existsSync(
        join(
          f.scratchHome,
          "Library",
          "LaunchAgents",
          `com.civilization.buzz-seat-clerk-${alias}.plist`,
        ),
      ),
      `expected ${alias}'s plist to be loaded`,
    );
  }
});

test("LCL-6: refuses loud, by name, for a seat with no generated plist in DEPLOY_DIR", () => {
  const f = makeFixture();
  const r = run(f, ["--live", "no-such-seat"]);
  assert.notEqual(r.status, 0, "must exit non-zero for an unknown seat");
  assert.match(r.stderr, /no-such-seat/);
});

test("PLISTKEY01 (MUTATION TARGET): the loaded plist under scratch HOME carries no credential -- no nsec1, no *_NSEC var name", () => {
  const f = makeFixture();
  run(f, ["--live", "--all"]);
  const laDir = join(f.scratchHome, "Library", "LaunchAgents");
  const files = existsSync(laDir) ? readdirSync(laDir) : [];
  assert.ok(files.length > 0, "expected loaded plists under scratch HOME");
  for (const file of files) {
    const text = readFileSync(join(laDir, file), "utf8");
    assert.doesNotMatch(
      text,
      /nsec1/,
      `MUTATION TARGET: ${file} must never contain an nsec1 literal`,
    );
    assert.doesNotMatch(
      text,
      /[A-Z0-9_]*_NSEC\b.*<string>/,
      `MUTATION TARGET: ${file} must never carry a *_NSEC key with an inline value`,
    );
  }
});
