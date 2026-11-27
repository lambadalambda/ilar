// Keeps the app shell (page, renderer, WebAssembly) on the phone, so the
// installed app opens without the network; the session itself always
// comes over iroh, never through here. The build stamps VERSION, and a
// new version drops the old cache.
const VERSION = "dev";
const CACHE = "ilar-remote-" + VERSION;
// The files the build wrote, stamped in by scripts/build-remote-web.sh.
const SHELL = [];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) => Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name))))
      .then(() => self.clients.claim()),
  );
});

// The network first, so a deploy shows up on the next open; the cache
// when there is none. "no-cache" asks the server each time: Pages says
// max-age=600, which would hold an old copy for ten minutes.
// Only this app's own files, and only good answers: a 404 during a
// deploy must not replace the cached shell, and nothing the wasm client
// fetches elsewhere belongs here.
self.addEventListener("fetch", (event) => {
  if (event.request.method !== "GET" || new URL(event.request.url).origin !== location.origin) return;
  event.respondWith(
    fetch(event.request, { cache: "no-cache" })
      .then((response) => {
        if (response.ok) {
          const copy = response.clone();
          caches.open(CACHE).then((cache) => cache.put(event.request, copy));
        }
        return response;
      })
      .catch(() => caches.match(event.request, { ignoreSearch: true })),
  );
});
