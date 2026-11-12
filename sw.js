// beleaf offline support: keeps the app on this device so it opens without internet. Version: 20261112-0353
const CACHE = 'beleaf-20261112-0353', SHELL = ['./', './index.html', './manifest.webmanifest', './icons/icon.svg', './icons/icon-192.png', './icons/icon-512.png', './icons/maskable-192.png', './icons/maskable-512.png', './icons/apple-touch-icon.png'];
self.addEventListener('install', e => { e.waitUntil(caches.open(CACHE).then(c => c.addAll(SHELL.map(u => new Request(u, { cache: 'reload' })))).then(() => self.skipWaiting())); });
self.addEventListener('activate', e => { e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k.startsWith('beleaf-') && k !== CACHE).map(k => caches.delete(k)))).then(() => self.clients.claim())); });
self.addEventListener('fetch', e => {
  const r = e.request; if (r.method !== 'GET') return;
  const u = new URL(r.url);
  // the test builds in /next/ and /engine-test/ are left alone (always fresh from the network)
  if (/\/(next|engine-test)\//.test(u.pathname)) return;
  // "Check for updates" asks the website directly, past this offline copy
  if (u.searchParams.has('fresh')) return;
  // the app itself: use the copy on this device, and quietly fetch the newest version for next time
  if (u.origin === location.origin) { e.respondWith(caches.open(CACHE).then(async c => { const hit = await c.match(r, { ignoreSearch: true }); const net = fetch(r).then(res => { if (res.ok) c.put(r, res.clone()); return res; }).catch(() => hit); return hit || net; })); return; }
  // fonts: keep a copy once loaded so text looks right offline too
  if (/fonts\.(googleapis|gstatic)\.com$/.test(u.hostname)) e.respondWith(caches.open(CACHE + '-fonts').then(async c => (await c.match(r)) || fetch(r).then(res => { c.put(r, res.clone()); return res; })));
});
