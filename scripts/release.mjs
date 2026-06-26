#!/usr/bin/env node
// Single-command release helper.
//
// Usage:
//   pnpm run release 0.6.0          explicit version
//   pnpm run release patch          0.5.2 -> 0.5.3
//   pnpm run release minor          0.5.2 -> 0.6.0
//   pnpm run release major          0.5.2 -> 1.0.0
//   pnpm run release 0.6.0 --no-git write the 3 files only, skip commit + tag
//   pnpm run release patch --force  skip dirty-tree / existing-tag pre-flight checks
//
// Writes the same version into package.json, src-tauri/Cargo.toml and
// src-tauri/tauri.conf.json, then commits and creates a matching v<version> tag.

import { readFileSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

function die(msg) {
  console.error(`✗ ${msg}`);
  process.exit(1);
}

// ---- parse args -----------------------------------------------------------
const args = process.argv.slice(2);
const noGit = args.includes('--no-git');
const force = args.includes('--force');
const positional = args.filter((a) => !a.startsWith('--'));

if (positional.length !== 1) {
  die('Expected exactly one version argument, e.g. `pnpm run release 0.6.0` or `pnpm run release patch`.');
}
const bump = positional[0];

// ---- resolve current + next version --------------------------------------
const pkgPath = join(root, 'package.json');
const pkgRaw = readFileSync(pkgPath, 'utf8');
const currentMatch = pkgRaw.match(/"version":\s*"(\d+\.\d+\.\d+)"/);
if (!currentMatch) {
  die('Could not read current version from package.json.');
}
const current = currentMatch[1];

let next;
if (['patch', 'minor', 'major'].includes(bump)) {
  const [major, minor, patch] = current.split('.').map(Number);
  if (bump === 'major') next = `${major + 1}.0.0`;
  else if (bump === 'minor') next = `${major}.${minor + 1}.0`;
  else next = `${major}.${minor}.${patch + 1}`;
} else if (/^\d+\.\d+\.\d+$/.test(bump)) {
  next = bump;
} else {
  die(`Invalid version "${bump}". Use a clean x.y.z (e.g. 0.6.0) or patch/minor/major.`);
}

if (next === current) {
  die(`New version (${next}) is the same as the current version.`);
}

const tag = `v${next}`;

// ---- git helpers ----------------------------------------------------------
function git(gitArgs, opts = {}) {
  return execFileSync('git', gitArgs, { cwd: root, encoding: 'utf8', ...opts }).trim();
}

// ---- pre-flight checks ----------------------------------------------------
if (!noGit && !force) {
  let status;
  try {
    status = git(['status', '--porcelain']);
  } catch {
    die('Not a git repository (or git is unavailable). Use --no-git to skip git steps.');
  }
  if (status) {
    die('Working tree is dirty. Commit or stash changes first, or pass --force.');
  }
  const tags = git(['tag', '--list', tag]);
  if (tags) {
    die(`Tag ${tag} already exists.`);
  }
}

// ---- write the three files ------------------------------------------------
// Targeted, format-preserving replacements. Two-phase: validate that every file
// has exactly one occurrence of the current version BEFORE writing anything, so
// a drifted file aborts the release without leaving a half-written tree.
const esc = current.replace(/\./g, '\\.');
const targets = [
  // package.json: "version": "x.y.z"
  { relPath: 'package.json', regex: new RegExp(`("version":\\s*")${esc}(")`) },
  // tauri.conf.json: "version": "x.y.z"
  { relPath: 'src-tauri/tauri.conf.json', regex: new RegExp(`("version":\\s*")${esc}(")`) },
  // Cargo.toml [package]: version = "x.y.z"
  { relPath: 'src-tauri/Cargo.toml', regex: new RegExp(`(^version\\s*=\\s*")${esc}(")`, 'm') },
];

console.log(`Releasing ${current} -> ${next}`);

// Phase 1: read + validate every file.
const planned = targets.map(({ relPath, regex }) => {
  const filePath = join(root, relPath);
  const raw = readFileSync(filePath, 'utf8');
  const globalRe = new RegExp(regex.source, regex.flags.includes('g') ? regex.flags : regex.flags + 'g');
  const matches = raw.match(globalRe);
  if (!matches || matches.length === 0) {
    die(`${relPath}: could not find version "${current}". Is it out of sync? Fix it to ${current} or pass --force.`);
  }
  if (matches.length > 1) {
    die(`${relPath}: found ${matches.length} matches for version "${current}"; refusing to guess.`);
  }
  return { relPath, filePath, updated: raw.replace(regex, `$1${next}$2`) };
});

// Phase 2: all validated — now write.
for (const { relPath, filePath, updated } of planned) {
  writeFileSync(filePath, updated);
  console.log(`  updated ${relPath}`);
}

// ---- git commit + tag -----------------------------------------------------
if (noGit) {
  console.log(`\nFiles written. Skipped git steps (--no-git).`);
  process.exit(0);
}

git(['add', 'package.json', 'src-tauri/Cargo.toml', 'src-tauri/tauri.conf.json']);
git(['commit', '-m', `Release ${tag}`]);
git(['tag', '-a', tag, '-m', tag]);

console.log(`\n✓ Committed and tagged ${tag}.`);
console.log(`  Push with: git push --follow-tags`);
