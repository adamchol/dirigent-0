// A session's machine, as the web shows and asks for it: the default one (the user's, where users
// have their own, over the cluster's), and what's sent when a new session asks for another.

import type { Machines, Resources, Settings } from "./api.ts";

/** The default machine of `settings`' user's new sessions. */
export function defaults(m: Machines, settings: Settings): Resources {
  const own = m.per_user ? (settings.machine ?? {}) : {};
  return {
    cpu: own.cpu ?? m.defaults.cpu,
    memory: own.memory ?? m.defaults.memory,
    disk: own.disk ?? m.defaults.disk,
  };
}

export function same(a: Resources, b: Resources): boolean {
  return a.cpu === b.cpu && a.memory === b.memory && a.disk === b.disk;
}

/** What a new session asks for, on `chosen`: nothing when it's the default one, which control
 * gives it then. */
export function asked(chosen: Resources, dflt: Resources): Resources | undefined {
  return same(chosen, dflt) ? undefined : chosen;
}

export function summary(r: Resources): string {
  return `${r.cpu} ${r.cpu === 1 ? "CPU" : "CPUs"} · ${r.memory} GiB memory · ${r.disk} GiB disk`;
}

/** The status bar's, short. */
export function brief(r: Resources): string {
  return `${r.cpu} CPU · ${r.memory} GiB`;
}

/** Why `r` can't be had, if it can't: what's under 1 or over the limits. */
export function wrong(r: Resources, limits: Partial<Resources>): string | null {
  for (const [part, name] of [
    ["cpu", "Cores"],
    ["memory", "Memory"],
    ["disk", "Disk"],
  ] as const) {
    const n = r[part];
    if (!Number.isInteger(n)) return `${name}: a whole number.`;
    if (n < 1) return `${name}: at least 1.`;
    const max = limits[part];
    if (max !== undefined && n > max) return `${name}: at most ${max}.`;
  }
  return null;
}
