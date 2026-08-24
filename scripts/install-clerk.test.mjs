/**
 * scripts/install-clerk.test.mjs
 *
 * comms-orch#24 item 4: the clerk binary must live at a stable path outside any git worktree
 * (a worktree can be removed or rebuilt out from under a running clerk process), installed via a
 * checksummed install step rather than a manual `cargo build` + copy. This tests install-clerk.sh
 * and verify-clerk-install.sh end-to-end against a hermetic fixture repo -- a real `cargo build`
 * is replaced with a stub `cargo` on PATH that writes a small fixed-content fake binary, so the
 * test is fast and does not depend on the real toolchain or the real (huge) monorepo build.
 *
 * Run with: node --test scripts/install-clerk.test.mjs
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, mkdirSync, rmSync, readFileSync, chmodSync, existsSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

const HERE = dirname(fileURLToPath(import.meta.url));
const INSTALL_SCRIPT = join(HERE, 'install-clerk.sh');
const VERIFY_SCRIPT = join(HERE, 'verify-clerk-install.sh');
const HARD_TIMEOUT_MS = 15_000;
const GIT_ENV = { GIT_AUTHOR_NAME: 'test', GIT_AUTHOR_EMAIL: 'test@example.com', GIT_COMMITTER_NAME: 'test', GIT_COMMITTER_EMAIL: 'test@example.com' };

function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

// Build a fixture "repo" that install-clerk.sh can run against: a real git repo (so
// `git rev-parse HEAD` / `git diff` work), a crates/buzz-seat-clerk dir (the dirty-check target),
// a copy of the two scripts under test, and a stub `cargo` on PATH standing in for the real build.
function withFixture(fn) {
  const repo = mkdtempSync(join(tmpdir(), 'install-clerk-repo-'));
  const binDir = mkdtempSync(join(tmpdir(), 'install-clerk-bin-'));
  const installDir = mkdtempSync(join(tmpdir(), 'install-clerk-target-'));

  mkdirSync(join(repo, 'crates', 'buzz-seat-clerk'), { recursive: true });
  writeFileSync(join(repo, 'crates', 'buzz-seat-clerk', 'marker.rs'), '// v1\n');
  mkdirSync(join(repo, 'scripts'), { recursive: true });
  writeFileSync(join(repo, 'scripts', 'install-clerk.sh'), readFileSync(INSTALL_SCRIPT));
  chmodSync(join(repo, 'scripts', 'install-clerk.sh'), 0o755);
  writeFileSync(join(repo, 'scripts', 'verify-clerk-install.sh'), readFileSync(VERIFY_SCRIPT));
  chmodSync(join(repo, 'scripts', 'verify-clerk-install.sh'), 0o755);

  const run = (...args) => spawnSync('git', args, { cwd: repo, encoding: 'utf8', env: { ...process.env, ...GIT_ENV }, timeout: HARD_TIMEOUT_MS });
  run('init', '--quiet');
  run('add', '-A');
  run('commit', '--quiet', '-m', 'init fixture');

  // Stub cargo: ignores its args, writes a fixed-content fake binary to $PWD/target/release/clerk.
  // CARGO_STUB_CONTENT lets a test change the "build output" between install runs.
  writeFileSync(join(binDir, 'cargo'), `#!/bin/sh
mkdir -p target/release
printf '%s' "\${CARGO_STUB_CONTENT:-fake-clerk-v1}" > target/release/clerk
chmod +x target/release/clerk
`);
  chmodSync(join(binDir, 'cargo'), 0o755);
  writeFileSync(join(binDir, 'shasum'), `#!/bin/sh
exec /usr/bin/shasum "$@"
`);
  chmodSync(join(binDir, 'shasum'), 0o755);

  const env = { ...process.env, PATH: `${binDir}:${process.env.PATH}` };
  const install = (extraEnv = {}) => spawnSync('bash', [join(repo, 'scripts', 'install-clerk.sh'), installDir], { cwd: repo, encoding: 'utf8', env: { ...env, ...extraEnv }, timeout: HARD_TIMEOUT_MS });
  const verify = () => spawnSync('bash', [join(repo, 'scripts', 'verify-clerk-install.sh'), installDir], { encoding: 'utf8', env, timeout: HARD_TIMEOUT_MS });
  const headSha = () => run('rev-parse', 'HEAD').stdout.trim();

  try {
    return fn({ repo, installDir, install, verify, headSha, run });
  } finally {
    rmSync(repo, { recursive: true, force: true });
    rmSync(binDir, { recursive: true, force: true });
    rmSync(installDir, { recursive: true, force: true });
  }
}

test('IC-1: install places an executable clerk binary at the install dir', () => {
  withFixture(({ install, installDir }) => {
    const r = install();
    assert.equal(r.status, 0, r.stderr);
    const binPath = join(installDir, 'clerk');
    assert.ok(existsSync(binPath));
    const mode = statSync(binPath).mode;
    assert.ok(mode & 0o111, 'binary must be executable');
  });
});

test('IC-2: the recorded sha256 matches the actual installed binary bytes', () => {
  withFixture(({ install, installDir }) => {
    const r = install();
    assert.equal(r.status, 0, r.stderr);
    const recorded = readFileSync(join(installDir, 'clerk.sha256'), 'utf8');
    const match = recorded.match(/^sha256: ([0-9a-f]{64})$/m);
    assert.ok(match, `no sha256 line in:\n${recorded}`);
    assert.equal(match[1], sha256File(join(installDir, 'clerk')));
  });
});

test('IC-3: the recorded source_commit matches the fixture repo HEAD', () => {
  withFixture(({ install, installDir, headSha }) => {
    const r = install();
    assert.equal(r.status, 0, r.stderr);
    const recorded = readFileSync(join(installDir, 'clerk.sha256'), 'utf8');
    assert.match(recorded, new RegExp(`^source_commit: ${headSha()}\\s*$`, 'm'));
  });
});

test('IC-4 (non-vacuity): a different build output produces a different recorded checksum, not a static value', () => {
  withFixture(({ install, installDir }) => {
    const r1 = install({ CARGO_STUB_CONTENT: 'fake-clerk-v1' });
    assert.equal(r1.status, 0, r1.stderr);
    const sha1 = readFileSync(join(installDir, 'clerk.sha256'), 'utf8').match(/^sha256: ([0-9a-f]{64})$/m)[1];

    const r2 = install({ CARGO_STUB_CONTENT: 'fake-clerk-v2-different-bytes' });
    assert.equal(r2.status, 0, r2.stderr);
    const sha2 = readFileSync(join(installDir, 'clerk.sha256'), 'utf8').match(/^sha256: ([0-9a-f]{64})$/m)[1];

    assert.notEqual(sha1, sha2, 'MUTATION TARGET: different binary bytes must produce a different recorded sha256');
    assert.equal(sha2, sha256File(join(installDir, 'clerk')), 'the second install must overwrite both the binary AND its checksum, not just append');
  });
});

test('IC-5: re-running with unchanged build output is idempotent (same checksum both times)', () => {
  withFixture(({ install, installDir }) => {
    const r1 = install();
    assert.equal(r1.status, 0, r1.stderr);
    const sha1 = readFileSync(join(installDir, 'clerk.sha256'), 'utf8').match(/^sha256: ([0-9a-f]{64})$/m)[1];
    const r2 = install();
    assert.equal(r2.status, 0, r2.stderr);
    const sha2 = readFileSync(join(installDir, 'clerk.sha256'), 'utf8').match(/^sha256: ([0-9a-f]{64})$/m)[1];
    assert.equal(sha1, sha2);
  });
});

test('IC-6: uncommitted changes under crates/buzz-seat-clerk are flagged dirty in the recorded source_commit', () => {
  withFixture(({ install, installDir, repo }) => {
    writeFileSync(join(repo, 'crates', 'buzz-seat-clerk', 'marker.rs'), '// v2 uncommitted\n');
    const r = install();
    assert.equal(r.status, 0, r.stderr);
    const recorded = readFileSync(join(installDir, 'clerk.sha256'), 'utf8');
    assert.match(recorded, /dirty: uncommitted changes under crates\/buzz-seat-clerk/);
  });
});

test('IC-7: install fails loud when the release binary is missing after the build step (build step wired wrong)', () => {
  withFixture(({ installDir, repo }) => {
    // A cargo stub that "succeeds" but produces nothing -- must not be treated as a successful install.
    const binDir = mkdtempSync(join(tmpdir(), 'install-clerk-badbin-'));
    writeFileSync(join(binDir, 'cargo'), '#!/bin/sh\nexit 0\n');
    chmodSync(join(binDir, 'cargo'), 0o755);
    const r = spawnSync('bash', [join(repo, 'scripts', 'install-clerk.sh'), installDir], {
      cwd: repo, encoding: 'utf8', timeout: HARD_TIMEOUT_MS,
      env: { ...process.env, PATH: `${binDir}:${process.env.PATH}` },
    });
    rmSync(binDir, { recursive: true, force: true });
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /not found/);
    assert.ok(!existsSync(join(installDir, 'clerk')), 'must not leave a stale/absent binary reported as installed');
  });
});

test('VC-1: verify passes against a fresh, untouched install', () => {
  withFixture(({ install, verify }) => {
    assert.equal(install().status, 0);
    const r = verify();
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /OK/);
  });
});

test('VC-2 (the whole point of the checksum): verify fails when the installed binary is corrupted/tampered after install', () => {
  withFixture(({ install, verify, installDir }) => {
    assert.equal(install().status, 0);
    writeFileSync(join(installDir, 'clerk'), 'tampered-bytes-not-what-was-installed');
    const r = verify();
    assert.notEqual(r.status, 0, 'MUTATION TARGET: a tampered binary must fail verification, not silently pass');
    assert.match(r.stderr, /mismatch/);
  });
});

test('VC-3: verify fails loud when the checksum file is missing', () => {
  withFixture(({ install, verify, installDir }) => {
    assert.equal(install().status, 0);
    rmSync(join(installDir, 'clerk.sha256'));
    const r = verify();
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /no checksum file/);
  });
});

test('VC-4: verify fails loud when the binary itself is missing', () => {
  withFixture(({ verify }) => {
    const r = verify();
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /no binary/);
  });
});

// REV-20260823-01 (root-caused own-hands by Overwatch/QA in opeff): `git -C <dir>` only changes
// the working DIRECTORY. It does NOT override GIT_DIR/GIT_WORK_TREE/etc when those are ambient in
// the environment, and git sets GIT_DIR for every hook it invokes. This reproduces that exact
// incident shape against install-clerk.sh itself: an ambient GIT_DIR pointing at a DECOY repo
// must not make `git -C "$REPO_ROOT" rev-parse HEAD` / `git diff` read the decoy.
test('IC-8 (REV-20260823-01 repro, MUTATION TARGET): an ambient GIT_DIR pointing at a decoy repo does not leak into the recorded source_commit', () => {
  withFixture(({ install, installDir, repo, headSha }) => {
    // A second, unrelated git repo standing in for "whatever repo GIT_DIR happens to point at"
    // -- e.g. a real checkout, if this script were ever invoked from inside one of its hooks.
    const decoy = mkdtempSync(join(tmpdir(), 'install-clerk-decoy-'));
    const runDecoy = (...args) => spawnSync('git', args, { cwd: decoy, encoding: 'utf8', env: { ...process.env, ...GIT_ENV }, timeout: HARD_TIMEOUT_MS });
    runDecoy('init', '--quiet');
    writeFileSync(join(decoy, 'decoy.txt'), 'decoy');
    runDecoy('add', '-A');
    runDecoy('commit', '--quiet', '-m', 'decoy commit');
    const decoyHead = runDecoy('rev-parse', 'HEAD').stdout.trim();

    const realHead = headSha();
    assert.notEqual(decoyHead, realHead, 'sanity: the decoy and real repo must have different HEADs, or this test proves nothing');

    // Simulate the exact incident vector: GIT_DIR ambient in the environment, pointing at the
    // decoy's .git, while install-clerk.sh is invoked with cwd/REPO_ROOT set to the real fixture.
    const r = install({ GIT_DIR: join(decoy, '.git') });
    assert.equal(r.status, 0, r.stderr);
    const recorded = readFileSync(join(installDir, 'clerk.sha256'), 'utf8');
    assert.match(recorded, new RegExp(`^source_commit: ${realHead}\\b`, 'm'), 'MUTATION TARGET: recorded source_commit must be the REAL repo\'s HEAD, never the ambient-GIT_DIR decoy\'s');
    assert.doesNotMatch(recorded, new RegExp(decoyHead), 'must never record the decoy repo\'s HEAD');

    rmSync(decoy, { recursive: true, force: true });
  });
});
