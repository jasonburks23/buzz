#!/usr/bin/env node
// check-clerk-secrets.mjs
//
// Run after generate-clerk-plists.mjs to confirm every live seat has its
// per-seat secret env file (containing SEAT_NSEC) before loading launchd plists.
//
// Usage:
//   node check-clerk-secrets.mjs [--registry <path>]
//
// Exit codes:
//   0  all env files present and non-empty
//   1  one or more env files missing or empty

import { readFileSync, existsSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { homedir } from 'node:os';

const args = process.argv.slice(2);
const arg = (flag, def) => {
  const i = args.indexOf(flag);
  return i !== -1 ? args[i + 1] : def;
};

const registryPath = arg(
  '--registry',
  resolve(
    homedir(),
    'Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json'
  )
);

const registry = JSON.parse(readFileSync(registryPath, 'utf8'));
const livSeats = (registry.seats ?? []).filter(s => s.status === 'live');

function slugify(tabName) {
  return tabName
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

let allOk = true;

for (const seat of livSeats) {
  const slug = slugify(seat.tabName);
  const envFile = join(homedir(), `.env.seat.${slug}`);

  if (!existsSync(envFile)) {
    console.error(`[check-secrets] MISSING: ${envFile}`);
    console.error(`  Create with: echo 'SEAT_NSEC=nsec1...' > ${envFile} && chmod 600 ${envFile}`);
    allOk = false;
    continue;
  }

  const content = readFileSync(envFile, 'utf8').trim();
  if (!content.startsWith('SEAT_NSEC=nsec1')) {
    console.error(`[check-secrets] INVALID: ${envFile} -- must start with SEAT_NSEC=nsec1`);
    allOk = false;
    continue;
  }

  const stat = statSync(envFile);
  const mode = (stat.mode & 0o777).toString(8);
  if (mode !== '600') {
    console.warn(`[check-secrets] WARNING: ${envFile} has mode ${mode}; recommend 600`);
    // Not fatal; warn only.
  }

  console.log(`[check-secrets] OK: ${envFile}`);
}

if (!allOk) {
  console.error('\n[check-secrets] One or more secret files are missing. Fix before loading plists.');
  process.exit(1);
}

console.log('\n[check-secrets] All secret files present. Safe to load plists.');
