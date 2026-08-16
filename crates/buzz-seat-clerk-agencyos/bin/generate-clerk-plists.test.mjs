// generate-clerk-plists.test.mjs
// TDD test harness for the plist generator.
// Run: node --test generate-clerk-plists.test.mjs
// Requires Node.js 20+ (built-in test runner, no npm deps).

import { test, describe } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

// Dynamic import so we can test the module before it fully exists.
// The test file lives next to the generator.
const __dir = dirname(fileURLToPath(import.meta.url));

// --- helpers ---

/**
 * Minimal fake registry with two live seats and one dormant seat.
 * CC Alpha has explicit buzzWakeFile / buzzReadackFile / buzzIdentityFile.
 * CC Beta relies on slug derivation (fields absent).
 */
const FAKE_REGISTRY = {
  buzz: {
    relayUrl: 'ws://localhost:3000',
    claimDir: '/tmp'
  },
  seats: [
    {
      tabName: 'AgencyOS (CC) Alpha',
      repoLocation: '/repos/agencyos-cc',
      role: 'AgencyOS-CC-Alpha',
      status: 'live',
      buzzWakeFile: '/tmp/buzz-clerk-wake-agencyos-cc-alpha.json',
      buzzReadackFile: '/tmp/buzz-clerk-readack-agencyos-cc-alpha.json',
      buzzIdentityFile: '/tmp/buzz-clerk-identity-agencyos-cc-alpha.json'
    },
    {
      tabName: 'AgencyOS (CC) Beta',
      repoLocation: '/repos/agencyos-cc-beta',
      role: 'AgencyOS-CC-Beta',
      status: 'live'
      // buzzWakeFile, buzzReadackFile, buzzIdentityFile intentionally absent
    },
    {
      tabName: 'Dormant Seat',
      repoLocation: '/repos/dormant',
      role: 'Dormant',
      status: 'dormant'
    }
  ]
};

// --- import generator module ---
const { generatePlists, slugify, deriveFilePath } = await import('./generate-clerk-plists.mjs');

describe('slugify', () => {
  test('converts AgencyOS (CC) Alpha to agencyos-cc-alpha', () => {
    assert.equal(slugify('AgencyOS (CC) Alpha'), 'agencyos-cc-alpha');
  });

  test('converts Operational Efficiency (Right Hand) to operational-efficiency-right-hand', () => {
    assert.equal(
      slugify('Operational Efficiency (Right Hand)'),
      'operational-efficiency-right-hand'
    );
  });

  test('collapses repeated hyphens', () => {
    assert.equal(slugify('A   B'), 'a-b');
  });

  test('trims leading and trailing hyphens', () => {
    assert.equal(slugify('(leading)'), 'leading');
  });
});

describe('deriveFilePath', () => {
  test('returns explicit value when field is present', () => {
    const result = deriveFilePath(
      { buzzWakeFile: '/explicit/path.json' },
      'buzzWakeFile',
      'buzz-clerk-wake',
      'agencyos-cc-alpha'
    );
    assert.equal(result, '/explicit/path.json');
  });

  test('derives /tmp/buzz-clerk-wake-agencyos-cc-beta.json when field absent', () => {
    const result = deriveFilePath(
      {},
      'buzzWakeFile',
      'buzz-clerk-wake',
      'agencyos-cc-beta'
    );
    assert.equal(result, '/tmp/buzz-clerk-wake-agencyos-cc-beta.json');
  });
});

describe('generatePlists', () => {
  const FAKE_BIN = '/usr/local/bin/clerk';
  const WRAPPER_DIR = '/tmp/buzz-clerk-wrappers';

  const plists = generatePlists(FAKE_REGISTRY, FAKE_BIN, WRAPPER_DIR);

  test('emits exactly two plists (dormant seat skipped)', () => {
    assert.equal(plists.length, 2);
  });

  test('alpha plist has correct label', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.ok(alpha, 'alpha plist not found');
    assert.match(alpha.xml, /com\.civilization\.buzz-clerk\.agencyos-cc-alpha/);
  });

  test('alpha plist sets RELAY_URL from global buzz.relayUrl', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /ws:\/\/localhost:3000/);
  });

  test('alpha plist sets CLAIM_DIR from global buzz.claimDir', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /<key>CLAIM_DIR<\/key>\s*<string>\/tmp<\/string>/);
  });

  test('alpha plist sets WAKE_FILE from explicit buzzWakeFile column', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /buzz-clerk-wake-agencyos-cc-alpha\.json/);
  });

  test('beta plist derives WAKE_FILE from slug when buzzWakeFile is absent', () => {
    const beta = plists.find(p => p.slug === 'agencyos-cc-beta');
    assert.ok(beta, 'beta plist not found');
    assert.match(beta.xml, /buzz-clerk-wake-agencyos-cc-beta\.json/);
  });

  test('SEAT_NSEC must NOT appear anywhere in any generated plist XML', () => {
    for (const plist of plists) {
      assert.doesNotMatch(
        plist.xml,
        /nsec1|SEAT_NSEC.*[a-zA-Z0-9]{20}/,
        `plist for ${plist.slug} contains a literal nsec value`
      );
      // The key itself is allowed to appear (it is referenced from the wrapper)
      // but no value should follow it in the plist body.
      // We check that no <string> immediately follows a SEAT_NSEC key with a real value.
      assert.doesNotMatch(
        plist.xml,
        /<key>SEAT_NSEC<\/key>\s*<string>[^<]{10,}<\/string>/,
        `plist for ${plist.slug} has a non-empty SEAT_NSEC string value`
      );
    }
  });

  test('plist references the wrapper script, not the clerk binary directly', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    // ProgramArguments must point to a wrapper script path, not the clerk binary.
    assert.match(alpha.xml, /buzz-clerk-wrappers/);
    assert.doesNotMatch(alpha.xml, new RegExp(`<string>${FAKE_BIN}</string>`));
  });

  test('plist has KeepAlive true', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /<key>KeepAlive<\/key>\s*<true\/>/);
  });

  test('plist has ThrottleInterval 30 matching compact-driver convention', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /<key>ThrottleInterval<\/key>\s*<integer>30<\/integer>/);
  });

  test('plist sets SEAT_ROLE from seat.role', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /<key>SEAT_ROLE<\/key>\s*<string>AgencyOS-CC-Alpha<\/string>/);
  });

  test('plist sets SEAT_CWD from seat.repoLocation', () => {
    const alpha = plists.find(p => p.slug === 'agencyos-cc-alpha');
    assert.match(alpha.xml, /<key>SEAT_CWD<\/key>\s*<string>\/repos\/agencyos-cc<\/string>/);
  });

  test('each plist has a distinct label (no label collision)', () => {
    const labels = plists.map(p => {
      const m = p.xml.match(/<key>Label<\/key>\s*<string>([^<]+)<\/string>/);
      return m ? m[1] : null;
    });
    const unique = new Set(labels);
    assert.equal(unique.size, plists.length, 'duplicate labels detected');
  });
});
