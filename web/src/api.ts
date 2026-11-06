// Control's HTTP API (control/src/main.rs), through the web server's `/api` proxy, as `Who`
// says: in control's `declared` auth the user chosen at the top (the `dirigent-user` header), in
// its `oidc` auth whoever signed in with Google (their ID token).

import type { Source } from "./events.ts";

export type Effort = "low" | "medium" | "high" | "max";
export type Part = "coder" | "dirigent";

export interface SessionSpec {
  model: string;
  effort: Effort | null;
  source?: Source | null;
  /** Absent: a coder. */
  part?: Part;
}

/** `SessionInfo` (protocol/src/control.rs): a session as its caller sees it. */
export interface SessionInfo {
  id: string;
  spec: SessionSpec;
  epoch: number;
  /** The player holding its lease; none, the session is idle. */
  player: { subject: string; attached: boolean } | null;
  /** Who made it: the user who writes to it, starts and stops it. */
  owner: string;
  /** Who else may read it. */
  viewers: string[];
  /** The caller's. */
  role: "owner" | "viewer";
  /** When it was made, in milliseconds since the Unix epoch. */
  created: number;
}

/** How control knows its callers (`GET /auth`). */
export type Auth = { mode: "declared" } | { mode: "oidc"; google_client_id: string };

/** Who calls: a subject; signed in, with the ID token that says so. */
export interface Who {
  user: string;
  idToken?: string;
}

export async function auth(): Promise<Auth> {
  const res = await fetch("/api/auth");
  if (!res.ok) throw new Error(`${res.status}: ${(await res.text()) || res.statusText}`);
  return res.json();
}

/** A chlive credential: the ID token, signed in; else, in `declared` auth, one control signs. */
export async function token(who: Who): Promise<string> {
  if (who.idToken) return who.idToken;
  return (await (await call(who, "POST", "/token")).json()).token;
}

export type State = "active" | "away" | "idle";

export function state(s: SessionInfo): State {
  return !s.player ? "idle" : s.player.attached ? "active" : "away";
}

export function part(s: SessionInfo): Part {
  return s.spec.part ?? "coder";
}

async function call(who: Who, method: string, path: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = who.idToken
    ? { authorization: `Bearer ${who.idToken}` }
    : { "dirigent-user": who.user };
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(`/api${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${res.status}: ${(await res.text()) || res.statusText}`);
  return res;
}

/** The sessions `who` owns or views. */
export async function sessions(who: Who): Promise<SessionInfo[]> {
  return (await call(who, "GET", "/sessions")).json();
}

/** Make the session `id`, `who`'s (or find it, with the same spec). */
export async function put(who: Who, id: string, spec: Partial<SessionSpec>, start: boolean) {
  await call(who, "PUT", `/sessions/${id}`, { ...spec, start });
}

export async function start(who: Who, id: string) {
  await call(who, "POST", `/sessions/${id}/start`);
}

/** `now`: cancel the running turn instead of letting it finish. */
export async function stop(who: Who, id: string, now: boolean) {
  await call(who, "POST", `/sessions/${id}/stop`, { now });
}

export async function addViewer(who: Who, id: string, viewer: string) {
  await call(who, "PUT", `/sessions/${id}/viewers/${encodeURIComponent(viewer)}`);
}

export async function removeViewer(who: Who, id: string, viewer: string) {
  await call(who, "DELETE", `/sessions/${id}/viewers/${encodeURIComponent(viewer)}`);
}

/** A git URL's repository as `owner/name`, or the URL without its scheme. */
export function repo(url: string): string {
  const path = url.replace(/^[a-z+]+:\/\/|^git@/, "").replace(/\.git$/, "");
  const parts = path.split(/[/:]/).filter(Boolean);
  return parts.length >= 3 ? parts.slice(-2).join("/") : path;
}

/** What a session is called: its repository (and ref), or what it is without one. */
export function title(s: SessionInfo): string {
  if (part(s) === "dirigent") return "Dirigent";
  const git = s.spec.source?.git;
  if (!git) return "Empty workspace";
  return git.ref ? `${repo(git.url)}@${git.ref}` : repo(git.url);
}
