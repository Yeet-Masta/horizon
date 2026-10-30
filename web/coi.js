// Cross-origin isolation from a service worker, for hosts that can't send headers (GitHub
// Pages, most static hosts): the threaded build's shared memory exists only on a page served
// with Cross-Origin-Opener-Policy and Cross-Origin-Embedder-Policy (web/serve.py and
// web/apache.htaccess send them). This worker adds them to the page's own responses; the page
// registers it only when it isn't isolated already (index.html), and reloads once it controls.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const request = event.request;
  // A request the browser may only answer from its cache can't be fetched again here.
  if (request.cache === "only-if-cached" && request.mode !== "same-origin") return;
  // Other origins' answers pass as they are: their own headers decide (the relay sends CORS).
  if (new URL(request.url).origin !== self.location.origin) return;
  event.respondWith(
    fetch(request).then((response) => {
      if (response.status === 0) return response;
      const headers = new Headers(response.headers);
      headers.set("Cross-Origin-Opener-Policy", "same-origin");
      headers.set("Cross-Origin-Embedder-Policy", "require-corp");
      headers.set("Cross-Origin-Resource-Policy", "same-origin");
      return new Response(response.body, {
        status: response.status,
        statusText: response.statusText,
        headers,
      });
    }),
  );
});
