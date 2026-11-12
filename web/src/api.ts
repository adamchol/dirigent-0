// Control's HTTP API (control/src/main.rs), through the web server's `/api` proxy, as `Who`
// says: in control's `declared` auth the user chosen at the top (the `dirigent-user` header), in
// its `oidc` auth whoever signed in with Google (their ID token).

import type { Named, Shown, Source } from "./events.ts";

export type Effort = "low" | "medium" | "high" | "max";
export type Part = "coder" | "dirigent";
/** Who a session's model calls are paid to: Anthropic, or its owner's ChatGPT plan. */
export type Provider = "anthropic" | "chatgpt";

export interface SessionSpec {
  model: string;
  effort: Effort | null;
  source?: Source | null;
  /** Absent: a coder. */
  part?: Part;
  /** Absent: Anthropic. */
  provider?: Provider;
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
  /** The names of the secrets its tools get (its own, and its owner's for all their sessions). */
  secrets?: string[];
  /** Since when it's archived (no player takes it until it's unarchived), in milliseconds since
   * the Unix epoch; none, it isn't. */
  archived?: number | null;
}

/** How control knows its callers (`GET /auth`). */
export type Auth = { mode: "declared" } | { mode: "oidc"; google_client_id: string };

/** The ChatGPT plan a user linked (through the CLI), and why it's broken, if it is. */
export interface Link {
  email: string;
  broken: string | null;
}

/** Who the caller is to control, their link, and their Anthropic key (`GET /me`). */
export interface Me {
  subject: string;
  chatgpt: Link | null;
  /** Their key's last four characters; none, they gave none. */
  anthropic?: { hint: string } | null;
}

/** A model a ChatGPT plan has. */
export interface Model {
  slug: string;
  display_name: string;
}

/** Who calls: a subject; signed in, with the token of their sign-in (control's, for it and chlive). */
export interface Who {
  user: string;
  token?: string;
}

export async function auth(): Promise<Auth> {
  const res = await fetch("/api/auth");
  if (!res.ok) throw new Error(`${res.status}: ${(await res.text()) || res.statusText}`);
  return res.json();
}

/** A chlive credential: the sign-in's token, signed in; else, in `declared` auth, one control signs. */
export async function token(who: Who): Promise<string> {
  if (who.token) return who.token;
  return (await (await call(who, "POST", "/token")).json()).token;
}

export type State = "active" | "away" | "idle";

export function state(s: SessionInfo): State {
  return !s.player ? "idle" : s.player.attached ? "active" : "away";
}

export function part(s: SessionInfo): Part {
  return s.spec.part ?? "coder";
}

export function provider(s: SessionInfo): Provider {
  return s.spec.provider ?? "anthropic";
}

async function call(who: Who, method: string, path: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = who.token
    ? { authorization: `Bearer ${who.token}` }
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

export async function me(who: Who): Promise<Me> {
  return (await call(who, "GET", "/me")).json();
}

/** The models `who`'s ChatGPT plan has. */
export async function models(who: Who): Promise<Model[]> {
  return (await call(who, "GET", "/links/chatgpt/models")).json();
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

/** Delete the session (archived): control removes it, and what it left, for good. */
export async function deleteSession(who: Who, id: string) {
  await call(who, "DELETE", `/sessions/${id}`);
}

/** Archive the session (`on`; it must be idle), or unarchive it. */
export async function archive(who: Who, id: string, on: boolean) {
  await call(who, "POST", `/sessions/${id}/${on ? "archive" : "unarchive"}`);
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

/** What a session is called: its title, once it has one, or else `place`'s. */
export function title(s: SessionInfo, named?: Named): string {
  return named?.title || place(s);
}

/** Where a session works: its repository (and ref), or what it is without one. */
export function place(s: SessionInfo): string {
  if (part(s) === "dirigent") return "Dirigent";
  const git = s.spec.source?.git;
  if (!git) return "Empty workspace";
  return git.ref ? `${repo(git.url)}@${git.ref}` : repo(git.url);
}

/** Give `who`'s Anthropic API key (control checks it with Anthropic first): its hint. */
export async function setAnthropicKey(who: Who, key: string): Promise<{ hint: string }> {
  return (await call(who, "PUT", "/links/anthropic", { key })).json();
}

export async function removeAnthropicKey(who: Who) {
  await call(who, "DELETE", "/links/anthropic");
}

/** Where secrets live: all `who`'s sessions (none), or one session's (its id). */
function secretsPath(session?: string): string {
  return session ? `/sessions/${session}/secrets` : "/secrets";
}

/** The names of the secrets, all sessions' or `session`'s own. */
export async function secrets(who: Who, session?: string): Promise<string[]> {
  return (await call(who, "GET", secretsPath(session))).json();
}

export async function setSecret(who: Who, name: string, value: string, session?: string) {
  await call(who, "PUT", `${secretsPath(session)}/${name}`, { value });
}

export async function removeSecret(who: Who, name: string, session?: string) {
  await call(who, "DELETE", `${secretsPath(session)}/${name}`);
}

/** Whether `name` may name a secret (control's rule): an environment variable's name. */
export function secretName(name: string): boolean {
  return /^[A-Z_][A-Z0-9_]{0,63}$/.test(name);
}

/** Whether control has a GitHub App, and the accounts whose installations of it `who` connected. */
export type GithubLink = { app: boolean; accounts: string[] };

export async function githubLink(who: Who): Promise<GithubLink> {
  return (await call(who, "GET", "/links/github")).json();
}

/** Where `who`'s browser goes to connect GitHub: installing the App, or (`existing`) authorizing it
 * for installations made already. GitHub sends it back to control, then here (`?github=…`). */
export async function connectGithub(who: Who, existing: boolean): Promise<string> {
  return (await call(who, "POST", "/links/github/start", { existing })).json().then((r) => r.url);
}

export async function unlinkGithub(who: Who) {
  await call(who, "DELETE", "/links/github");
}

/** Whether `who`'s sessions can read a repository, and how: `repo` is null for one not on GitHub;
 * `via` null for none, `why` saying what GitHub refused; `app`, whether there's an App to connect. */
export type Access = {
  repo: string | null;
  via?: "public" | "token" | "app" | null;
  why?: string | null;
  app: boolean;
};

export async function githubAccess(who: Who, url: string): Promise<Access> {
  return (await call(who, "GET", `/github/access?url=${encodeURIComponent(url)}`)).json();
}

/** Whether `url` is a repository on github.com (as control parses them). */
export function onGithub(url: string): boolean {
  return /^(https?:\/\/(www\.)?github\.com\/|git@github\.com:|ssh:\/\/git@github\.com\/)[\w.-]+\/[\w.-]+?(\.git)?\/?$/.test(url.trim());
}

/** `who`'s settings, as control keeps them (`control/src/settings.rs`). */
export type Settings = {
  /** How much of a session's log the chat shows. */
  events: Shown;
};

export async function settings(who: Who): Promise<Settings> {
  return (await call(who, "GET", "/settings")).json();
}

/** Keep `s` as `who`'s settings, all of them: control's answer. */
export async function saveSettings(who: Who, s: Settings): Promise<Settings> {
  return (await call(who, "PUT", "/settings", s)).json();
}

/** The system the players' VMs run: the image's stock one, unless the cluster has a configuration of
 * its own (the chart's `player.nixos`), built ahead of time. */
export type PlayerSystem = {
  configured: boolean;
  state?: "building" | "ready" | "failed" | "broken";
  store_path?: string;
  error?: string;
};

export async function playerSystem(who: Who): Promise<PlayerSystem> {
  return (await call(who, "GET", "/system")).json();
}
