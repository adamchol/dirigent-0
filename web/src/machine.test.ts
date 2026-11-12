import { expect, test } from "bun:test";

import type { Machines } from "./api.ts";
import { asked, defaults, summary, wrong } from "./machine.ts";

const cluster: Machines = { defaults: { cpu: 2, memory: 4, disk: 20 }, limits: { cpu: 8 }, per_user: false, warm: 1 };

test("a user's default machine is their own over the cluster's, where users have their own", () => {
  const settings = { events: "activity" as const, machine: { memory: 16 } };
  expect(defaults(cluster, settings)).toEqual({ cpu: 2, memory: 4, disk: 20 });
  expect(defaults({ ...cluster, per_user: true }, settings)).toEqual({ cpu: 2, memory: 16, disk: 20 });
  expect(defaults({ ...cluster, per_user: true }, { events: "activity" })).toEqual(cluster.defaults);
});

test("a new session asks for a machine only when it isn't the default one", () => {
  const d = cluster.defaults;
  expect(asked({ ...d }, d)).toBeUndefined();
  expect(asked({ ...d, cpu: 4 }, d)).toEqual({ cpu: 4, memory: 4, disk: 20 });
});

test("a machine is told, and checked against the limits", () => {
  expect(summary({ cpu: 1, memory: 2, disk: 10 })).toBe("1 CPU · 2 GiB memory · 10 GiB disk");
  expect(wrong({ cpu: 8, memory: 64, disk: 100 }, cluster.limits)).toBeNull();
  expect(wrong({ cpu: 9, memory: 4, disk: 20 }, cluster.limits)).toBe("Cores: at most 8.");
  expect(wrong({ cpu: 2, memory: 0, disk: 20 }, cluster.limits)).toBe("Memory: at least 1.");
  expect(wrong({ cpu: 2, memory: 1.5, disk: 20 }, cluster.limits)).toBe("Memory: a whole number.");
});
