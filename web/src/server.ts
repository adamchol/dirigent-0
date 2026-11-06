// The web UI's server: the app (index.html, which Bun bundles), and two proxies so the browser
// talks to one origin: `/api` to control's HTTP API, `/live` to chlive's WebSocket.
//
// In development the app is Bun's dev server (hot reloading), which serves only requests made to
// it by its own address (no DNS rebinding): it runs on a port of its own, and this server
// forwards to it as that address, so it can be reached by any name (`tailscale serve`).

import index from "./index.html";

const control = process.env.CONTROL ?? "http://127.0.0.1:8090";
const chlive = process.env.CHLIVE_URL ?? "ws://127.0.0.1:8080/live";
const development = process.env.NODE_ENV !== "production";

/** A browser's WebSocket passed on: where to, as whom, and what came before it opened. */
interface Relay {
  url: string;
  origin?: string;
  upstream?: WebSocket;
  early: (string | Buffer)[];
}

/** Bun's WebSocket client, which takes headers: the DOM's type (the app's, in tsconfig.json) doesn't say so. */
const BunWebSocket = WebSocket as unknown as new (url: string, options?: Bun.WebSocketOptions) => WebSocket;

const app = development
  ? Bun.serve({
      id: "app",
      hostname: "127.0.0.1",
      port: 0,
      routes: { "/*": index },
      development: { hmr: true, console: true },
    })
  : undefined;

const server = Bun.serve<Relay>({
  id: "web",
  hostname: process.env.HOST ?? "127.0.0.1",
  port: Number(process.env.PORT ?? 3000),
  // the app it serves (production) is bundled, not hot-reloaded: that's the dev server's
  development: false,
  routes: {
    "/api/*": api,
    "/live": (req: Request, server: Bun.Server<Relay>) => relay(req, server, chlive),
    // the dev server's hot reloading socket
    ...(app && { "/_bun/hmr": (req: Request, server: Bun.Server<Relay>) => relay(req, server, wsUrl(app.url), app.url.origin) }),
    "/*": app ? (req: Request) => forward(req, app.url) : index,
  },
  websocket: {
    open(ws) {
      const upstream = new BunWebSocket(ws.data.url, ws.data.origin ? { headers: { origin: ws.data.origin } } : undefined);
      upstream.binaryType = "arraybuffer";
      ws.data.upstream = upstream;
      upstream.onopen = () => {
        for (const m of ws.data.early) upstream.send(m);
        ws.data.early = [];
      };
      upstream.onmessage = (e) => ws.send(e.data);
      // the far end's own codes (chlive's 4401: credentials refused) reach the client
      upstream.onclose = (e) => ws.close(sendable(e.code), e.reason);
    },
    message(ws, m) {
      const upstream = ws.data.upstream!;
      if (upstream.readyState === WebSocket.OPEN) upstream.send(m);
      else ws.data.early.push(m);
    },
    close(ws, code, reason) {
      ws.data.upstream?.close(sendable(code), reason);
    },
  },
});

console.log(`web at ${server.url}, control at ${control}, chlive at ${chlive}${app ? `, the dev server at ${app.url}` : ""}`);

/** `/api/<path>` as control's `/<path>`. */
function api(req: Request): Promise<Response> {
  const url = new URL(req.url);
  return pass(req, new URL(url.pathname.slice("/api".length) + url.search, control));
}

/** A request to the dev server, as if made to it. */
function forward(req: Request, to: URL): Promise<Response> {
  const url = new URL(req.url);
  const headers = new Headers(req.headers);
  if (headers.has("origin")) headers.set("origin", to.origin);
  return pass(req, new URL(url.pathname + url.search, to), headers);
}

function pass(req: Request, to: URL, headers = new Headers(req.headers)): Promise<Response> {
  headers.delete("host");
  if (to.host) headers.set("host", to.host);
  return fetch(to, {
    method: req.method,
    headers,
    body: req.body,
    redirect: "manual",
    // passed on as it came
    decompress: false,
  });
}

/** Upgrade to a WebSocket passed on to `url` (with the request's query), as `origin`. */
function relay(req: Request, server: Bun.Server<Relay>, url: string, origin?: string): Response | undefined {
  const data: Relay = { url: url + new URL(req.url).search, origin, early: [] };
  return server.upgrade(req, { data }) ? undefined : new Response("a WebSocket only", { status: 426 });
}

function wsUrl(http: URL): string {
  return `${http.protocol === "https:" ? "wss" : "ws"}://${http.host}/_bun/hmr`;
}

/** A close code one may send: those the protocol keeps for itself (1005, 1006, 1015: no code,
 * dropped, TLS) become 1011, which the client takes as a reason to reconnect. */
function sendable(code: number): number {
  return code === 1000 || (code >= 1001 && code <= 1014 && ![1004, 1005, 1006].includes(code)) || (code >= 3000 && code < 5000)
    ? code
    : 1011;
}
