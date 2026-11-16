// Minimal service worker: caches the app shell (styles, scripts, icons) so the installed
// app opens instantly. Pages and API calls always go to the network: they are private
// and must never be served stale.
const CACHE = "sitewright-shell-v6";
const SHELL = [
  "/static/app.css",
  "/static/app.js",
  "/static/passkey.js",
  "/static/run.js",
  "/static/dashboard.js",
  "/static/vendor/htmx.min.js",
  "/static/icon.svg",
  "/static/icon-192.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))).then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.origin !== self.location.origin || !url.pathname.startsWith("/static/")) {
    return; // default network handling
  }
  // Stale-while-revalidate for the shell.
  event.respondWith(
    caches.open(CACHE).then(async (cache) => {
      const cached = await cache.match(event.request);
      const fresh = fetch(event.request).then((response) => {
        if (response.ok) cache.put(event.request, response.clone());
        return response;
      }).catch(() => cached);
      return cached || fresh;
    })
  );
});
