import { expect, test } from "bun:test";

import { timeline, type EventRow } from "./events.ts";

let offset = 0;
function row(id: string, kind: string, data?: unknown, author = "player:p"): EventRow {
  return {
    session_id: "s",
    offset: offset++,
    partition: 0,
    ts: "2026-11-03 12:00:00.000",
    id,
    author,
    kind,
    data: data === undefined ? "" : JSON.stringify(data),
  };
}

const usage = (input: number, output: number) => ({ input, output, cache_read: 0, cache_write: 0 });

test("a tool call is shown with its result, and the turn ends with its usage", () => {
  const t = timeline([
    row("m", "user_message", { text: "list" }, "user:dev"),
    row("t", "turn_started", { input: "m" }),
    row("a1", "assistant_message", {
      content: [
        { type: "thinking", text: "hm" },
        { type: "tool_call", id: "c", name: "bash", input: { command: "ls" } },
      ],
      stop_reason: { reason: "tool_use" },
      usage: usage(10, 2),
    }),
    row("r", "tool_result", { call_id: "c", name: "bash", content: "a\nb", is_error: false }),
    row("a2", "assistant_message", {
      content: [{ type: "text", text: "two files" }],
      stop_reason: { reason: "end_turn" },
      usage: usage(20, 3),
    }),
    row("e", "turn_ended", { reason: "completed" }),
  ]);
  expect(t.openTurn).toBeUndefined();
  expect(t.items.map((i) => i.t)).toEqual(["user", "thinking", "tool", "text", "turn"]);
  expect(t.items[0]).toMatchObject({ author: "user:dev", text: "list", queued: false });
  expect(t.items[2]).toMatchObject({ name: "bash", input: { command: "ls" }, result: { content: "a\nb", isError: false } });
  expect(t.items[4]).toMatchObject({ reason: "completed", interrupted: false, usage: usage(30, 5) });
});

test("a turn without its end is open, and its tool call waits; a message no turn took is queued", () => {
  const t = timeline([
    row("m", "user_message", { text: "go" }, "user:dev"),
    row("t", "turn_started", { input: "m" }),
    row("a", "assistant_message", {
      content: [{ type: "tool_call", id: "c", name: "bash", input: { command: "sleep 9" } }],
      stop_reason: { reason: "tool_use" },
      usage: usage(1, 1),
    }),
    row("m2", "user_message", { text: "and then" }, "user:dev"),
  ]);
  expect(t.openTurn).toBe("t");
  expect(t.items[1]).toMatchObject({ t: "tool", name: "bash" });
  expect(t.items[1]).not.toHaveProperty("result");
  expect(t.items[2]).toMatchObject({ t: "user", queued: true });
});

test("an interrupted turn says so; a detached player's turn isn't open", () => {
  const t = timeline([
    row("m", "user_message", { text: "go" }, "user:dev"),
    row("t", "turn_started", { input: "m" }),
    row("i", "user_interrupt", { turn: "t" }, "user:dev"),
    row("c", "model_cancelled"),
    row("e", "turn_ended", { reason: "aborted", interrupt: "i" }),
    row("m2", "user_message", { text: "again" }, "user:dev"),
    row("t2", "turn_started", { input: "m2" }),
    row("d", "player_detached", { epoch: 2 }),
  ]);
  expect(t.items.find((i) => i.t === "turn")).toMatchObject({ reason: "aborted", interrupted: true });
  expect(t.items.find((i) => i.key === "c")).toMatchObject({ t: "note", tone: "dim" });
  expect(t.openTurn).toBeUndefined();
});

test("failures are errors, and an unreadable row doesn't stop the rest", () => {
  const bad = { ...row("x", "user_message"), data: "{" };
  const t = timeline([bad, row("f", "player_failed", { epoch: 1, error: "no clone" })]);
  expect(t.items).toEqual([
    { t: "note", key: "x", tone: "error", text: "unreadable user_message event", detail: "{" },
    { t: "note", key: "f", tone: "error", text: "player failed: no clone", detail: undefined },
  ]);
});

test("a dirigent's message says via whom, and a session update is an input a turn takes", () => {
  const t = timeline([
    row("m", "user_message", { text: "go", via: "d1" }, "user:dev"),
    row("u", "session_update", {
      session: "s2",
      offset: 9,
      update: { what: "turn_ended", reason: "completed" },
      reply: "done",
    }),
    row("u2", "session_update", { session: "s2", offset: 12, update: { what: "player_detached" } }),
    row("t", "turn_started", { input: "u" }),
  ]);
  expect(t.items[0]).toMatchObject({ t: "user", via: "d1", queued: true });
  expect(t.items[1]).toMatchObject({ t: "update", session: "s2", what: "turn completed", reply: "done", queued: false });
  expect(t.items[2]).toMatchObject({ t: "update", what: "stopped", queued: true });
});
