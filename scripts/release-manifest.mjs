#!/usr/bin/env node
// Write the update manifests for a GitHub release from the collected build outputs (run by the release job in CI):
// - latest.json: Tauri updater format for the desktop app (tauri-plugin-updater reads it from releases/latest/download/latest.json)
// - latest-android.json: read by the Android app's own updater (MainActivity.kt), with the APK's sha256
// Files are expected to be named by the desktop / android jobs: yi-console_<version>_<platform>[-setup.exe|.app.tar.gz|...].
// The .sig files are only inputs (their content goes into latest.json) and are deleted afterwards.
// Usage: node scripts/release-manifest.mjs <dir> <tag> <owner/repo> [notes file]

import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [dir, tag, repo, notesFile] = process.argv.slice(2);
if (!dir || !tag || !repo) {
  console.error('usage: release-manifest.mjs <dir> <tag> <owner/repo> [notes file]');
  process.exit(1);
}
const version = tag.replace(/^v/, '');
const [major, minor, patch] = version.split('.').map(Number);
const notes = notesFile && existsSync(notesFile) ? readFileSync(notesFile, 'utf8').trim() : '';
const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
const url = (name) => `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(name)}`;
const files = readdirSync(dir);

// Updater platform key -> file suffix of the bundle the updater installs
const targets = {
  'windows-x86_64': 'windows-x86_64-setup.exe',
  'darwin-aarch64': 'macos-aarch64.app.tar.gz',
  'darwin-x86_64': 'macos-x86_64.app.tar.gz',
  'linux-x86_64': 'linux-x86_64.AppImage',
};
const platforms = {};
for (const [key, suffix] of Object.entries(targets)) {
  const name = `yi-console_${version}_${suffix}`;
  if (!files.includes(name)) continue;
  if (!files.includes(`${name}.sig`)) {
    console.error(`${name} 没有签名文件（.sig），检查 TAURI_SIGNING_PRIVATE_KEY 是否配置`);
    process.exit(1);
  }
  platforms[key] = { url: url(name), signature: readFileSync(join(dir, `${name}.sig`), 'utf8').trim() };
}
if (!Object.keys(platforms).length) {
  console.error('没有找到任何桌面端更新包');
  process.exit(1);
}
writeFileSync(join(dir, 'latest.json'), JSON.stringify({ version, notes, pub_date: pubDate, platforms }, null, 2) + '\n');
console.log(`latest.json: ${Object.keys(platforms).join(', ')}`);

const apk = `yi-console_${version}_android.apk`;
if (files.includes(apk)) {
  const data = readFileSync(join(dir, apk));
  const android = {
    version,
    // Same formula Tauri uses for the APK's versionCode
    versionCode: major * 1000000 + minor * 1000 + patch,
    notes,
    pub_date: pubDate,
    url: url(apk),
    sha256: createHash('sha256').update(data).digest('hex'),
    size: statSync(join(dir, apk)).size,
  };
  writeFileSync(join(dir, 'latest-android.json'), JSON.stringify(android, null, 2) + '\n');
  console.log(`latest-android.json: ${apk}`);
}

for (const f of files) if (f.endsWith('.sig')) unlinkSync(join(dir, f));
