// Control's HTTP API (control/src/main.rs), through the dev server's `/api` proxy.

import type { Source } from "./events.ts";

export type Effort = "low" | "medium" | "high" | "max";

export interface SessionSpec {
  model: string;
  effort: Effort | null;
  source?: Source | null;
}

/** `SessionInfo` (protocol/src/control.rs). */
export interface SessionInfo {
  id: string;
  spec: SessionSpec;
  epoch: number;
  /** The player holding its lease; none, the session is idle. */
  player: { subject: string; attached: boolean } | null;
}

export type State = "active" | "away" | "idle";

export function state(s: SessionInfo): State {
  return !s.player ? "idle" : s.player.attached ? "active" : "away";
}

async function call(method: string, path: string, body?: unknown): Promise<Response> {
  const res = await fetch(`/api${path}`, {
    method,
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${res.status}: ${(await res.text()) || res.statusText}`);
  return res;
}

export async function sessions(): Promise<SessionInfo[]> {
  return (await call("GET", "/sessions")).json();
}

/** Make the session `id` (or find it, with the same spec), with access for `users`. */
export async function put(id: string, spec: Partial<SessionSpec>, users: string[], start: boolean) {
  await call("PUT", `/sessions/${id}`, { ...spec, users, start });
}

export async function start(id: string) {
  await call("POST", `/sessions/${id}/start`);
}

/** `now`: cancel the running turn instead of letting it finish. */
export async function stop(id: string, now: boolean) {
  await call("POST", `/sessions/${id}/stop`, { now });
}
