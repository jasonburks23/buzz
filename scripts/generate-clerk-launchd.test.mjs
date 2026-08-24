/**
 * scripts/generate-clerk-launchd.test.mjs
 *
 * comms-orch#18: wiring test for generate-clerk-launchd.sh against a fixture registry -- proves
 * the LIVE script (registry reads, preflight, file writes), not just the pure lib functions
 * (clerk-launchd-lib.test.mjs). Never touches the real fleet registry, the real installed clerk
 * binary, or launchctl. See clerk-launchd-daemon.test.mjs for the real-launchd acceptance test.
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
  existsSync,
  readdirSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCRIPT = join(HERE, "generate-clerk-launchd.sh");
const HARD_TIMEOUT_MS = 15_000;

function makeFixture({ withClerkBin = true } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "gcl-fixture-"));
  const envLocal = join(dir, ".env.local");
  writeFileSync(
    envLocal,
    "OVERWATCH_NSEC=nsec1thisisatestvaluenotrealsecret\n",
  );

  const registry = {
    buzz: { relayUrl: "https://relay.example.invalid" },
    fleet_boot: { envLocal },
    seats: [
      {
        alias: "overwatch",
        role: "AgencyOS-Overwatch",
        tabName: "Overwatch",
        buzzKeyEnvVar: "OVERWATCH_NSEC",
        sessionId: "sess-abc",
        tpRole: "overwatch",
        buzzChannels: ["general"],
      },
      {
        // Not bootable: no buzzChannels -- 2026-08-24 roster restructure shape (a CC seat taken
        // off comms entirely). Must be skipped, loud, by name -- never silently.
        alias: "dormant-seat",
        role: "AgencyOS-Dormant",
        tabName: "Dormant",
        buzzKeyEnvVar: "DORMANT_NSEC",
        sessionId: "sess-xyz",
        tpRole: "dormant",
        buzzChannels: [],
      },
      {
        // Not bootable: has a room, but buzzKeyEnvVar points at a var envLocal never defines --
        // the 2026-08-24 new-TP-seat shape (placeholder pubkey, no real key provisioned yet).
        // Must refuse loud, by name, never emit a plist that would load with an empty key.
        alias: "new-tp-seat",
        role: "AgencyOS-NewTP",
        tabName: "NewTP",
        buzzKeyEnvVar: "NEWTP_NSEC_NOT_YET_PROVISIONED",
        sessionId: "sess-newtp",
        tpRole: "newtp",
        buzzChannels: ["general"],
      },
    ],
  };
  const registryPath = join(dir, "fleet-seat-registry.json");
  writeFileSync(registryPath, JSON.stringify(registry, null, 2));

  const installDir = join(dir, "install");
  mkdirSync(installDir, { recursive: true });
  if (withClerkBin) {
    writeFileSync(join(installDir, "clerk"), "#!/bin/sh\nexit 0\n");
    chmodSync(join(installDir, "clerk"), 0o755);
  }

  const deployDir = join(dir, "deploy");
  const logDir = join(dir, "logs");
  mkdirSync(logDir, { recursive: true });

  return { dir, registryPath, installDir, deployDir, logDir };
}

function run(fixture) {
  return spawnSync("bash", [SCRIPT], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      SEAT_REGISTRY_PATH: fixture.registryPath,
      CLERK_INSTALL_DIR: fixture.installDir,
      CLERK_LAUNCHD_DEPLOY_DIR: fixture.deployDir,
      CLERK_LOG_DIR: fixture.logDir,
    },
  });
}

test("GCL-1 (MUTATION TARGET): generates exactly one plist + wrapper for the bootable seat, none for the non-bootable one", () => {
  const f = makeFixture();
  const r = run(f);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);

  const plistPath = join(
    f.deployDir,
    "com.civilization.buzz-seat-clerk-overwatch.plist",
  );
  const wrapperPath = join(f.deployDir, "run-clerk-overwatch.sh");
  assert.ok(
    existsSync(plistPath),
    `MUTATION TARGET: expected plist at ${plistPath}`,
  );
  assert.ok(
    existsSync(wrapperPath),
    `MUTATION TARGET: expected wrapper at ${wrapperPath}`,
  );

  assert.ok(
    !existsSync(
      join(f.deployDir, "com.civilization.buzz-seat-clerk-dormant-seat.plist"),
    ),
    "MUTATION TARGET: a non-bootable seat (no buzzChannels) must not get a job",
  );
  assert.ok(
    !existsSync(
      join(f.deployDir, "com.civilization.buzz-seat-clerk-new-tp-seat.plist"),
    ),
    "MUTATION TARGET: a seat with no real key must not get a job",
  );
});

test("GCL-6 (MUTATION TARGET): a retired (no-channels) seat is skipped LOUD, by name, with a named reason -- never silently", () => {
  const f = makeFixture();
  const r = run(f);
  assert.equal(
    r.status,
    0,
    `expected overall success (one bootable seat), got:\n${r.stdout}\n${r.stderr}`,
  );
  assert.match(
    r.stderr,
    /SKIP dormant-seat/,
    `MUTATION TARGET: stderr must name the skipped seat, got:\n${r.stderr}`,
  );
  assert.match(
    r.stderr,
    /dormant-seat.*not in any room/i,
    `MUTATION TARGET: stderr must name WHY, got:\n${r.stderr}`,
  );
});

test("GCL-7 (MUTATION TARGET): a seat with no real key is skipped LOUD, by name, with a named reason -- never silently, and never with an empty-key plist", () => {
  const f = makeFixture();
  const r = run(f);
  assert.equal(
    r.status,
    0,
    `expected overall success (one bootable seat), got:\n${r.stdout}\n${r.stderr}`,
  );
  assert.match(
    r.stderr,
    /SKIP new-tp-seat/,
    `MUTATION TARGET: stderr must name the skipped seat, got:\n${r.stderr}`,
  );
  assert.match(
    r.stderr,
    /new-tp-seat.*NO KEY CONFIGURED/i,
    `MUTATION TARGET: stderr must name WHY (no key), got:\n${r.stderr}`,
  );
});

test("GCL-8: a mixed roster (one bootable, one retired, one unprovisioned) generates exactly the bootable seat and reports accurate skip counts", () => {
  const f = makeFixture();
  const r = run(f);
  assert.equal(r.status, 0, r.stderr);
  assert.match(
    r.stdout,
    /1 job\(s\) written/,
    `expected exactly 1 job written, got:\n${r.stdout}`,
  );
  assert.match(r.stdout, /1 skipped: no channels/, r.stdout);
  assert.match(r.stdout, /1 skipped: no key/, r.stdout);
});

test("GCL-2: the generated plist points ProgramArguments at the wrapper, with RunAtLoad+KeepAlive true", () => {
  const f = makeFixture();
  run(f);
  const plistPath = join(
    f.deployDir,
    "com.civilization.buzz-seat-clerk-overwatch.plist",
  );
  const parse = spawnSync(
    "python3",
    [
      "-c",
      `
import plistlib, sys
d = plistlib.load(open(sys.argv[1], 'rb'))
assert d['Label'] == 'com.civilization.buzz-seat-clerk-overwatch', d
assert d['ProgramArguments'][0].endswith('run-clerk-overwatch.sh'), d
assert d['RunAtLoad'] is True, d
assert d['KeepAlive'] is True, d
print('OK')
`,
      plistPath,
    ],
    { encoding: "utf8", timeout: HARD_TIMEOUT_MS },
  );
  assert.equal(parse.stdout.trim(), "OK", parse.stderr);
});

test("GCL-3: the generated wrapper never contains the real secret value, only the KEYVAR name", () => {
  const f = makeFixture();
  run(f);
  const wrapper = readFileSync(
    join(f.deployDir, "run-clerk-overwatch.sh"),
    "utf8",
  );
  assert.match(wrapper, /export SEAT_NSEC="\$\{OVERWATCH_NSEC\}"/);
  assert.doesNotMatch(
    wrapper,
    /nsec1thisisatestvaluenotrealsecret/,
    "MUTATION TARGET: the real secret value must never be written into a generated artifact",
  );
});

test("GCL-4 (MUTATION TARGET): refuses to generate anything when the canonical clerk binary is missing", () => {
  const f = makeFixture({ withClerkBin: false });
  const r = run(f);
  assert.notEqual(
    r.status,
    0,
    "must exit non-zero when the canonical binary is absent",
  );
  assert.match(r.stderr, /canonical clerk binary not found/i);
  assert.ok(
    !existsSync(f.deployDir) || readdirSync(f.deployDir).length === 0,
    "must not write partial artifacts when the preflight fails",
  );
});

test("GCL-5: never invokes launchctl as a live command -- only ever prints it as operator instructions", () => {
  // Real proof: run the generator against a fixture, capture every launchctl on PATH, assert it
  // was never actually invoked. Stubbing launchctl (rather than grepping source text) survives a
  // future refactor of how the instructions are worded.
  const f = makeFixture();
  const stubDir = mkdtempSync(join(tmpdir(), "gcl-launchctl-stub-"));
  const callLog = join(stubDir, "calls.log");
  writeFileSync(
    join(stubDir, "launchctl"),
    `#!/bin/sh\necho "$@" >> "${callLog}"\n`,
  );
  chmodSync(join(stubDir, "launchctl"), 0o755);

  const r = spawnSync("bash", [SCRIPT], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      PATH: `${stubDir}:${process.env.PATH}`,
      SEAT_REGISTRY_PATH: f.registryPath,
      CLERK_INSTALL_DIR: f.installDir,
      CLERK_LAUNCHD_DEPLOY_DIR: f.deployDir,
      CLERK_LOG_DIR: f.logDir,
    },
  });
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);
  assert.ok(
    !existsSync(callLog),
    `MUTATION TARGET: launchctl must never actually be invoked by this generator -- that is operator hands. Calls seen: ${existsSync(callLog) ? readFileSync(callLog, "utf8") : ""}`,
  );
});
