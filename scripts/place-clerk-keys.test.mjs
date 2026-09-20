/**
 * scripts/place-clerk-keys.test.mjs
 *
 * opeff#1227: wiring test for place-clerk-keys.sh against a fixture registry and a scratch env
 * file. Never touches the real fleet registry or the real canonical env file.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  writeFileSync,
  mkdirSync,
  readFileSync,
  existsSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCRIPT = join(HERE, "place-clerk-keys.sh");
const HARD_TIMEOUT_MS = 15_000;
const FIXTURE_SECRET = "nsec1thisisatestvaluenotrealsecret";

function mode(path) {
  return (statSync(path).mode & 0o777).toString(8);
}

function makeFixture() {
  const dir = mkdtempSync(join(tmpdir(), "pck-fixture-"));
  const envLocal = join(dir, ".env.local");
  writeFileSync(
    envLocal,
    `OVERWATCH_NSEC=${FIXTURE_SECRET}\nUNPROVISIONED_NSEC=\n`,
  );

  const registry = {
    fleet_boot: { envLocal },
    seats: [
      { alias: "overwatch", tabName: "Overwatch", buzzKeyEnvVar: "OVERWATCH_NSEC" },
      { alias: "no-key-seat", tabName: "NoKeySeat", buzzKeyEnvVar: "NEVER_DEFINED_NSEC" },
      { alias: "empty-key-seat", tabName: "EmptyKeySeat", buzzKeyEnvVar: "UNPROVISIONED_NSEC" },
    ],
  };
  const registryPath = join(dir, "fleet-seat-registry.json");
  writeFileSync(registryPath, JSON.stringify(registry, null, 2));

  const keysDir = join(dir, "keys");

  return { dir, registryPath, envLocal, keysDir };
}

function run(fixture, args = []) {
  return spawnSync("bash", [SCRIPT, ...args], {
    encoding: "utf8",
    timeout: HARD_TIMEOUT_MS,
    env: {
      ...process.env,
      SEAT_REGISTRY_PATH: fixture.registryPath,
      CLERK_KEYS_DIR: fixture.keysDir,
    },
  });
}

test("PCK-1: dry run (default) writes nothing under the scratch keys dir", () => {
  const f = makeFixture();
  const r = run(f);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);
  assert.ok(!existsSync(f.keysDir), "dry run must not create the keys dir at all");
  assert.match(r.stdout, /would write/);
});

test("PCK-2 (MUTATION TARGET): dry run never prints the fixture secret value anywhere in stdout or stderr", () => {
  const f = makeFixture();
  const r = run(f);
  assert.doesNotMatch(
    r.stdout,
    new RegExp(FIXTURE_SECRET),
    `MUTATION TARGET: stdout must never contain the secret value, got:\n${r.stdout}`,
  );
  assert.doesNotMatch(
    r.stderr,
    new RegExp(FIXTURE_SECRET),
    `MUTATION TARGET: stderr must never contain the secret value, got:\n${r.stderr}`,
  );
});

test("PCK-3 (MUTATION TARGET): --live writes a mode-600 header file under the scratch keys dir, with the right variable and value", () => {
  const f = makeFixture();
  const r = run(f, ["--live"]);
  assert.equal(r.status, 0, `expected success, got:\n${r.stdout}\n${r.stderr}`);

  const target = join(f.keysDir, "overwatch.env");
  assert.ok(existsSync(target), `expected header file at ${target}`);
  assert.equal(
    mode(target),
    "600",
    `MUTATION TARGET: header file must be mode 600, got ${mode(target)}`,
  );
  assert.equal(
    mode(f.keysDir),
    "700",
    `MUTATION TARGET: keys dir must be mode 700, got ${mode(f.keysDir)}`,
  );

  const content = readFileSync(target, "utf8");
  assert.equal(content, `OVERWATCH_NSEC=${FIXTURE_SECRET}\n`);
});

test("PCK-4 (MUTATION TARGET): --live never prints the secret value to stdout, even though it writes it to disk", () => {
  const f = makeFixture();
  const r = run(f, ["--live"]);
  assert.doesNotMatch(
    r.stdout,
    new RegExp(FIXTURE_SECRET),
    `MUTATION TARGET: --live's own stdout must never echo the value it just wrote, got:\n${r.stdout}`,
  );
});

test("PCK-5 (MUTATION TARGET): a seat whose keyvar name is not in the env file at all is refused by name, not written", () => {
  const f = makeFixture();
  const r = run(f, ["--live"]);
  assert.match(r.stderr, /REFUSE no-key-seat/, r.stderr);
  assert.ok(!existsSync(join(f.keysDir, "no-key-seat.env")));
});

test("PCK-6 (MUTATION TARGET): a seat whose keyvar is present but empty is refused by name, not written", () => {
  const f = makeFixture();
  const r = run(f, ["--live"]);
  assert.match(r.stderr, /REFUSE empty-key-seat/, r.stderr);
  assert.ok(!existsSync(join(f.keysDir, "empty-key-seat.env")));
});

test("PCK-7: a second --live run overwrites the same file idempotently, still mode 600", () => {
  const f = makeFixture();
  run(f, ["--live"]);
  const r2 = run(f, ["--live"]);
  assert.equal(r2.status, 0, r2.stderr);
  const target = join(f.keysDir, "overwatch.env");
  assert.equal(mode(target), "600");
  assert.equal(
    readFileSync(target, "utf8"),
    `OVERWATCH_NSEC=${FIXTURE_SECRET}\n`,
  );
});
