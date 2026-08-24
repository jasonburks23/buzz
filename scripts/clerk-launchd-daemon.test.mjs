/**
 * scripts/clerk-launchd-daemon.test.mjs
 *
 * comms-orch#18 acceptance test: proves the REAL launchd mechanism, not just that a plist parses
 * or a process happens to have ppid 1. Two traps Overwatch named explicitly, both guarded against
 * here:
 *
 *   Trap 1 (this morning's near-miss): a process with ppid 1 is NOT proof launchd is managing it
 *   -- an orphan reparented after its shell died looks identical by that one signal alone (pid
 *   11980, ppid 1, launchctl list showed no job at all). This test instead checks `launchctl list
 *   <label>` for job PRESENCE under its own Label, and separately kills the running process and
 *   proves launchd -- not this test -- brings a NEW pid back under that same label. A KeepAlive
 *   key that has never been observed to revive anything is not KeepAlive.
 *
 *   Trap 2 (comms-orch#31's root cause): a correct file on disk is not a running process. The
 *   installed clerk binary sat correct and unused for 18 hours while every live clerk ran a
 *   two-day-old debug build from a worktree. This test asserts the pid launchd reports is
 *   actually EXECUTING the exact fixture "canonical" binary path (via `ps -o command=`, the full
 *   invocation -- `ps -o comm=` alone reports only the interpreter, "bash", for a shebang script,
 *   which would pass for ANY bash script, canonical or not), both before and after the
 *   kill-and-revive -- never just that the plist's ProgramArguments string looks right.
 *
 * Uses the REAL scripts/generate-clerk-launchd.sh to produce the wrapper + plist (the same code
 * path production uses), against a fixture registry and a throwaway fixture "clerk" binary (never
 * the real compact-driver.js-class binary, never a real seat). Loads and unloads a real launchd
 * job under a uniquely-tokened THROWAWAY label via the real `launchctl`, and removes it in every
 * teardown path (success, failure, or a hung wait). CLERKS ARE OPERATOR-OWNED: this file never
 * touches a live seat's label, pid, or plist -- only ones it creates and destroys itself, under a
 * label that can never collide with a real seat's `com.civilization.buzz-seat-clerk-<alias>`.
 *
 * macOS only (launchd). Skips (not fails) if launchctl bootstrap cannot reach a GUI domain (e.g.
 * a headless CI runner with no Aqua session) -- that is an environment limitation, not a defect.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  writeFileSync,
  mkdirSync,
  chmodSync,
  existsSync,
  rmSync,
} from "node:fs";
import { tmpdir, platform } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { randomBytes } from "node:crypto";

const HERE = dirname(fileURLToPath(import.meta.url));
const GENERATE_SCRIPT = join(HERE, "generate-clerk-launchd.sh");
const HARD_TIMEOUT_MS = 30_000;
const UID_ = process.getuid();
const GUI_DOMAIN = `gui/${UID_}`;

const RUN_TOKEN = `co18${process.pid}${Date.now()}${randomBytes(4).toString("hex")}`;
const ALIAS = `co18test-${RUN_TOKEN}`;
const LABEL = `com.civilization.buzz-seat-clerk-${ALIAS}`;

function sh(cmd, args, opts = {}) {
  return spawnSync(cmd, args, {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    ...opts,
  });
}

function launchctlList(label) {
  const r = sh("launchctl", ["list", label]);
  return r.status === 0 ? r.stdout : null;
}

function pidFromLaunchctlList(label) {
  const out = launchctlList(label);
  if (!out) return null;
  const m = out.match(/"PID"\s*=\s*(\d+);/);
  return m ? Number(m[1]) : null;
}

function commandOf(pid) {
  const r = sh("ps", ["-o", "command=", "-p", String(pid)]);
  return (r.stdout || "").trim();
}

function isAlive(pid) {
  const r = sh("ps", ["-o", "stat=", "-p", String(pid)]);
  const stat = (r.stdout || "").toString().trim();
  return stat.length > 0 && !stat.startsWith("Z");
}

function waitFor(predicate, timeoutMs = HARD_TIMEOUT_MS, intervalMs = 300) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeoutMs) {
    last = predicate();
    if (last) return last;
    sh("sleep", [String(intervalMs / 1000)]);
  }
  throw new Error(`waitFor: timed out after ${timeoutMs}ms`);
}

// Best-effort teardown: unload the throwaway job (bootout), ignoring any error (job may already
// be gone, or bootstrap may never have succeeded). Never touches any label but this file's own.
function teardownJob() {
  sh("launchctl", ["bootout", `${GUI_DOMAIN}/${LABEL}`]);
}

function makeFixture() {
  const dir = mkdtempSync(join(tmpdir(), `co18-${RUN_TOKEN}-`));
  const envLocal = join(dir, ".env.local");
  writeFileSync(envLocal, "CO18TEST_NSEC=nsec1co18testfixturevaluenotreal\n");

  const registry = {
    buzz: { relayUrl: "https://relay.example.invalid" },
    fleet_boot: { envLocal },
    seats: [
      {
        alias: ALIAS,
        role: `AgencyOS-${ALIAS}`,
        tabName: ALIAS,
        buzzKeyEnvVar: "CO18TEST_NSEC",
        sessionId: "sess-co18test",
        tpRole: ALIAS,
        buzzChannels: ["general"],
      },
    ],
  };
  const registryPath = join(dir, "fleet-seat-registry.json");
  writeFileSync(registryPath, JSON.stringify(registry, null, 2));

  // The fixture "canonical" clerk binary: never the real clerk, never a real seat. Blocks via the
  // `read` builtin against a private fifo (no forked grandchild -- same non-forking discipline as
  // comms-orch#11 slice C's throwaway supervisor children), so a SIGKILL to this one pid is a
  // complete, clean kill with nothing left behind for launchd (or this test) to reap separately.
  const installDir = join(dir, "install");
  mkdirSync(installDir, { recursive: true });
  const clerkBinPath = join(installDir, "clerk");
  const fifoPath = join(dir, "clerk.fifo");
  writeFileSync(
    clerkBinPath,
    `#!/usr/bin/env bash
FIFO="${fifoPath}"
[ -p "$FIFO" ] || mkfifo "$FIFO"
exec 3<> "$FIFO"
while true; do read -t 3600 -u 3 _; done
`,
  );
  chmodSync(clerkBinPath, 0o755);

  const deployDir = join(dir, "deploy");
  const logDir = join(dir, "logs");
  mkdirSync(logDir, { recursive: true });

  return { dir, registryPath, installDir, clerkBinPath, deployDir, logDir };
}

test("CLD-1 (MUTATION TARGET, real launchd, kills+revives): launchd relaunches a killed clerk process under the SAME label, still executing the canonical binary path", async (t) => {
  if (platform() !== "darwin") {
    t.skip(
      "launchd is macOS-only; this repo's CI runs on Linux, so this test cannot execute there -- run it by hand on macOS before every gate on comms-orch#18",
    );
    return;
  }
  const f = makeFixture();
  t.after(() => {
    teardownJob();
    rmSync(f.dir, { recursive: true, force: true });
  });

  const gen = spawnSync("bash", [GENERATE_SCRIPT], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      SEAT_REGISTRY_PATH: f.registryPath,
      CLERK_INSTALL_DIR: f.installDir,
      CLERK_LAUNCHD_DEPLOY_DIR: f.deployDir,
      CLERK_LOG_DIR: f.logDir,
    },
  });
  assert.equal(
    gen.status,
    0,
    `generate-clerk-launchd.sh failed:\n${gen.stdout}\n${gen.stderr}`,
  );

  const plistPath = join(f.deployDir, `${LABEL}.plist`);
  assert.ok(existsSync(plistPath), `expected generated plist at ${plistPath}`);

  const bootstrap = sh("launchctl", ["bootstrap", GUI_DOMAIN, plistPath]);
  if (
    bootstrap.status !== 0 &&
    /Bootstrap failed: 5|no GUI session|Operation not permitted/i.test(
      bootstrap.stderr || "",
    )
  ) {
    t.skip(
      `launchctl bootstrap could not reach ${GUI_DOMAIN} in this environment (no GUI/Aqua session) -- not a code defect: ${bootstrap.stderr}`,
    );
    return;
  }
  assert.equal(
    bootstrap.status,
    0,
    `launchctl bootstrap failed:\n${bootstrap.stdout}\n${bootstrap.stderr}`,
  );

  // Trap 1 guard: presence under launchctl list, by Label -- never inferred from ppid alone.
  const firstPid = waitFor(() => pidFromLaunchctlList(LABEL));
  assert.ok(
    firstPid,
    `MUTATION TARGET: launchctl list ${LABEL} must report a PID after bootstrap+RunAtLoad`,
  );
  assert.ok(isAlive(firstPid), "the reported pid must actually be alive");

  // Trap 2 guard, stated exactly the way Overwatch framed it: prove the RUNNING process is
  // executing that exact canonical path, not that a plist merely points at it. Checks the full
  // command line (not just `ps -o comm=`, which reports the interpreter -- "bash" -- for a
  // shebang script and would pass for ANY bash script, canonical or not) contains the fixture
  // clerk binary's exact path. Also guards against launchd's own transient "xpcproxy" spawn stage
  // seen briefly right after bootstrap, before it execs through to the target -- poll until it
  // settles, not a single early snapshot.
  const firstComm = waitFor(() => {
    const c = commandOf(firstPid);
    return c.includes(f.clerkBinPath) ? c : null;
  });
  assert.ok(
    firstComm.includes(f.clerkBinPath),
    `MUTATION TARGET: the launchd-managed process must be executing the exact canonical binary path (${f.clerkBinPath}) -- a wrapper that never execs would show its own script path instead, got: "${firstComm}"`,
  );

  // The actual non-vacuity bar: kill the real running process (never the job, never a live seat)
  // and prove LAUNCHD -- not this test -- brings a new one back under the same label.
  process.kill(firstPid, "SIGKILL");
  waitFor(() => !isAlive(firstPid));

  const secondPid = waitFor(() => {
    const p = pidFromLaunchctlList(LABEL);
    return p && p !== firstPid ? p : null;
  }, HARD_TIMEOUT_MS);
  assert.notEqual(
    secondPid,
    firstPid,
    "MUTATION TARGET: launchd must relaunch under a NEW pid -- the same pid reappearing would mean this never actually died",
  );
  assert.ok(isAlive(secondPid), "the revived pid must be alive");

  const secondComm = waitFor(() => {
    const c = commandOf(secondPid);
    return c.includes(f.clerkBinPath) ? c : null;
  });
  assert.ok(
    secondComm.includes(f.clerkBinPath),
    `the REVIVED process must also be executing the canonical binary path (${f.clerkBinPath}), got: "${secondComm}"`,
  );

  // Clean revival, not a straggler: the original pid must be gone by the time the new one is up.
  assert.equal(
    isAlive(firstPid),
    false,
    "the original (killed) pid must not still be lingering",
  );
});

test("CLD-2: teardown leaves no job under this run's throwaway label and no process carrying its RUN_TOKEN", (t) => {
  if (platform() !== "darwin") {
    t.skip("launchd is macOS-only");
    return;
  }
  const stillListed = launchctlList(LABEL);
  assert.equal(
    stillListed,
    null,
    `MUTATION TARGET: the throwaway job must be fully unloaded after teardown, launchctl list still shows:\n${stillListed}`,
  );

  const survivors = sh("pgrep", ["-f", RUN_TOKEN]).stdout.trim();
  assert.equal(
    survivors,
    "",
    `process(es) carrying RUN_TOKEN=${RUN_TOKEN} survived teardown (pids: ${survivors.replace(/\n/g, ",")})`,
  );
});
