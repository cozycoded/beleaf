// Prepares desktop/dist and the Tauri settings for one flavour of the app, before a build:
//   node flavor.js main   -> beleaf       (the official site's index.html)
//   node flavor.js next   -> beleaf next  (next/index.html with its Rust engine; installs beside beleaf, keeps its own drawings)
const fs = require('fs'), path = require('path');
const flavor = process.argv[2] || 'main';
const here = __dirname, root = path.join(here, '..'), dist = path.join(here, 'dist');
const cp = (from, to) => { fs.mkdirSync(path.dirname(to), { recursive: true }); fs.copyFileSync(from, to); };
const cpDir = (from, to) => { for (const f of fs.readdirSync(from)) { const s = path.join(from, f); fs.statSync(s).isDirectory() ? cpDir(s, path.join(to, f)) : cp(s, path.join(to, f)); } };

fs.rmSync(dist, { recursive: true, force: true });
cpDir(path.join(root, 'icons'), path.join(dist, 'icons'));
cp(path.join(root, 'index.html'), path.join(dist, 'index.html'));
if (flavor === 'next') {
  // same layout as the website (/next/index.html, /next/engine/…, /icons/…) so every path inside the page still works
  cpDir(path.join(root, 'next'), path.join(dist, 'next'));
  const edit = (file, fn) => { const p = path.join(here, file); const j = JSON.parse(fs.readFileSync(p, 'utf8')); fn(j); fs.writeFileSync(p, JSON.stringify(j, null, 2)); };
  edit('src-tauri/tauri.conf.json', j => {
    j.productName = 'beleaf next';
    j.identifier = 'com.cozycoded.beleaf.next';
    j.bundle.shortDescription = 'beleaf next drawing app';
    for (const w of j.app.windows) { w.title = 'beleaf next'; w.url = 'next/index.html'; }
  });
  edit('src-tauri/tauri.android.conf.json', j => {
    j.identifier = 'com.cozycoded.beleaf.next';
    for (const w of j.app.windows) { w.title = 'beleaf next'; w.url = 'next/index.html'; }
  });
  const ma = path.join(here, 'android/MainActivity.kt');
  fs.writeFileSync(ma, fs.readFileSync(ma, 'utf8').replace(/^package .*$/m, 'package com.cozycoded.beleaf.next'));
}
console.log('prepared', flavor, fs.readdirSync(dist));
