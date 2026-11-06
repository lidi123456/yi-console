#!/usr/bin/env node
// Stamp the release version into every place it is read from, so all builds of one release agree:
// - app/src-tauri/tauri.conf.json: the app version the updater compares, and Android's versionName / versionCode
// - Cargo.toml [workspace.package]: yi-agentd / client version (DAEMON_VERSION = <version>+<commit>)
// - app/src-tauri/Cargo.toml, app/package.json: kept in step
// - Cargo.lock, app/package-lock.json: the workspace's own entries, so `--locked` builds and `npm ci` agree
// Usage: node scripts/set-version.mjs v1.2.3   (a leading "v" is optional); prints the bare version.
// CI runs this on tag builds with the tag name. To raise the version in the repository, run it locally and commit.

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (process.argv[2] ?? '').trim();
const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(arg);
if (!m) {
  console.error(`版本号格式不对: "${arg}"，应为 v<major>.<minor>.<patch>，例如 v0.2.0`);
  process.exit(1);
}
const [major, minor, patch] = m.slice(1).map(Number);
// Tauri derives Android's versionCode as major*1000000 + minor*1000 + patch, which must keep increasing
if (minor > 999 || patch > 999 || major > 2000) {
  console.error('minor / patch 不能超过 999，major 不能超过 2000（Android versionCode 由它们算出）');
  process.exit(1);
}
const version = `${major}.${minor}.${patch}`;

function edit(rel, fn) {
  const p = join(root, rel);
  const before = readFileSync(p, 'utf8');
  const after = fn(before);
  if (after === before && !before.includes(version)) {
    console.error(`没能在 ${rel} 里改到版本号`);
    process.exit(1);
  }
  writeFileSync(p, after);
}

// The first `version = "…"` line: [workspace.package] in the workspace, [package] in the app crate
const firstVersionLine = (s) => s.replace(/^version = "[^"]*"$/m, `version = "${version}"`);
edit('Cargo.toml', firstVersionLine);
edit('app/src-tauri/Cargo.toml', firstVersionLine);
// Only the version lines change, so a committed bump leaves the files' formatting alone
const topVersion = (s) => s.replace(/^(  "version": )"[^"]*"/m, `$1"${version}"`);
edit('app/src-tauri/tauri.conf.json', topVersion);
edit('app/package.json', topVersion);
// The root package and its "" entry; dependencies keep their own versions
edit('app/package-lock.json', (s) => topVersion(s).replace(/("": \{\n\s+"name": "[^"]*",\n\s+"version": )"[^"]*"/, `$1"${version}"`));
// Workspace members (they use version.workspace); third-party crates keep their versions
const members = /^members = \[(.*)\]$/m.exec(readFileSync(join(root, 'Cargo.toml'), 'utf8'))?.[1] ?? '';
const crates = [...members.matchAll(/"([^"]+)"/g)].map(([, dir]) => /^name = "([^"]+)"$/m.exec(readFileSync(join(root, dir, 'Cargo.toml'), 'utf8'))?.[1]);
edit('Cargo.lock', (s) => crates.reduce((acc, name) => acc.replace(new RegExp(`(^name = "${name}"\\nversion = )"[^"]*"`, 'm'), `$1"${version}"`), s));
console.log(version);
