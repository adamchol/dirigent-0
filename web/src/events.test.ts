import { expect, test } from "bun:test";

import { callStart, contextSize, startup, streamed, timeline, titles, type EventRow } from "./events.ts";

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
    { t: "note", key: "x", kind: "user_message", tone: "error", text: "unreadable user_message event", detail: "{" },
    { t: "note", key: "f", kind: "player_failed", tone: "error", text: "player failed: no clone", detail: undefined },
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

test("deltas make the reply being written; its call starts after the last boundary", () => {
  const d = (delta: unknown, offset: number) => ({ offset, id: `d${offset}`, data: JSON.stringify({ delta }) });
  const blocks = streamed([
    d({ type: "block_start", index: 0, block: { type: "thinking", text: "" } }, 10),
    d({ type: "thinking", index: 0, text: "hm, " }, 11),
    d({ type: "thinking", index: 0, text: "ok" }, 12),
    d({ type: "block_start", index: 1, block: { type: "text", text: "" } }, 13),
    d({ type: "text", index: 1, text: "Hello " }, 14),
    d({ type: "text", index: 1, text: "there" }, 15),
    d({ type: "block_start", index: 2, block: { type: "tool_call", id: "c", name: "Bash", input: {} } }, 16),
    d({ type: "tool_call_args", index: 2, json: '{"command": "l' }, 17),
    d({ type: "tool_call_args", index: 2, json: '{"command": "ls"}' }, 18),
    d({ type: "usage", input: 1, output: 1, cache_read: 0, cache_write: 0 }, 19),
  ]);
  expect(blocks).toEqual([
    { t: "thinking", key: "stream:0", text: "hm, ok" },
    { t: "text", key: "stream:1", text: "Hello there" },
    { t: "tool", key: "stream:2", name: "Bash", input: { command: "ls" } },
  ]);

  offset = 0;
  const rows = [
    row("m", "user_message", { text: "hi" }, "user:dev"),
    row("t", "turn_started", { input: "m" }),
    row("u", "user_message", { text: "and" }, "user:dev"),
  ];
  expect(callStart(rows)).toBe(1);
});

test("the context is the last model call's input and output", () => {
  offset = 0;
  const call = (id: string, input: number, cached: number, output: number) =>
    row(id, "assistant_message", {
      content: [{ type: "text", text: "ok" }],
      stop_reason: { reason: "end_turn" },
      usage: { input, output, cache_read: cached, cache_write: 0 },
    });
  expect(contextSize([])).toBe(0);
  expect(contextSize([call("a", 100, 0, 10), call("b", 20, 110, 5), row("e", "turn_ended", { reason: "completed" })])).toBe(135);
});

test("a title is a note, a held one a rename, and the latest per session names it", () => {
  const t = timeline([
    row("n1", "titled", { title: "Fix login", description: "Makes it pass." }),
    row("n2", "titled", { title: "Mine", description: "As I named it.", hold: true }, "user:dev"),
  ]);
  expect(t.items.map((i) => (i.t === "note" ? [i.text, i.detail] : i.t))).toEqual([
    ["titled: Fix login", "Makes it pass."],
    ["renamed: Mine", "As I named it."],
  ]);
  const named = titles([
    { session_id: "a", offset: 7, data: JSON.stringify({ title: "Mine", description: "As I named it.", hold: true }) },
    { session_id: "a", offset: 3, data: JSON.stringify({ title: "Fix login", description: "Makes it pass." }) },
    { session_id: "b", offset: 1, data: "not json" },
    { session_id: "c", offset: 2, data: JSON.stringify({ title: "", description: "untitled" }) },
  ]);
  expect([...named.keys()]).toEqual(["a"]);
  expect(named.get("a")?.hold).toBe(true);
});

test("a dirigent's request for a secret is a card its answer settles; a refused one says why", () => {
  const ask = (id: string, session: string) => ({
    type: "tool_call",
    id,
    name: "request_secret",
    input: { session, name: "GITHUB_TOKEN", why: "to push" },
  });
  const t = timeline([
    row("a", "assistant_message", {
      content: [ask("c1", "s1"), ask("c2", "s2")],
      stop_reason: { reason: "tool_use" },
      usage: usage(1, 1),
    }),
    row("r1", "tool_result", { call_id: "c1", name: "request_secret", content: "Asked", is_error: false }),
    row("r2", "tool_result", { call_id: "c2", name: "request_secret", content: "you only view it", is_error: true }),
    row("ans", "secret_answered", { session: "s1", name: "GITHUB_TOKEN", provided: true, call: "c1" }, "user:dev"),
    row("ch", "secrets_changed", { names: ["GITHUB_TOKEN", "NPM_TOKEN"] }),
  ]);
  const requests = t.items.filter((i) => i.t === "request");
  expect(requests).toEqual([
    {
      t: "request",
      key: "c1",
      session: "s1",
      name: "GITHUB_TOKEN",
      why: "to push",
      result: { content: "Asked", isError: false },
      answer: "provided",
    },
    {
      t: "request",
      key: "c2",
      session: "s2",
      name: "GITHUB_TOKEN",
      why: "to push",
      result: { content: "you only view it", isError: true },
    },
  ]);
  const notes = t.items.flatMap((i) => (i.t === "note" ? [i.text] : []));
  expect(notes).toEqual(["provided: GITHUB_TOKEN", "secrets: GITHUB_TOKEN, NPM_TOKEN"]);
  // not a tool step
  expect(t.items.some((i) => i.t === "tool")).toBe(false);
});

test("a starting session waits on its machine, its workspace, then the turn", () => {
  const asked = [row("m", "user_message", { text: "go" }, "user:dev")];
  // control holds the lease for epoch 2's player, which hasn't attached
  expect(startup(asked, 2)?.phase).toBe("machine");
  const attached = [...asked, row("a", "player_attached", { epoch: 2 })];
  expect(startup(attached, 2)?.phase).toBe("workspace");
  expect(startup(attached, 2, "o/r")?.label).toBe("Cloning o/r…");
  expect(startup(attached, 2)?.label).toBe("Preparing the workspace…");
  const ready = [...attached, row("w", "workspace_ready", {})];
  expect(startup(ready, 2)?.phase).toBe("turn");
  // the turn takes it: the timeline's own "Working…" from there
  expect(startup([...ready, row("t", "turn_started", { input: "m" })], 2)).toBeNull();
  // idle, it waits on nothing
  expect(startup(ready, null)).toBeNull();
});

test("a session with a checkpoint restores its workspace, and an earlier player's is no one's", () => {
  const log = [
    row("a1", "player_attached", { epoch: 1 }),
    row("w1", "workspace_ready", {}),
    row("m", "user_message", { text: "go" }, "user:dev"),
    row("t", "turn_started", { input: "m" }),
    row("e", "turn_ended", { reason: "completed" }),
    row("c", "checkpoint", { turn: "t", snapshot: "k" }),
    row("d", "player_detached", { epoch: 1 }),
  ];
  expect(startup(log, 2)?.phase).toBe("machine");
  const back = [...log, row("a2", "player_attached", { epoch: 2 })];
  expect(startup(back, 2)?.label).toBe("Restoring the workspace from its checkpoint…");
  // nothing queued once it's restored: waiting for a message, not starting
  expect(startup([...back, row("r", "workspace_restored", { snapshot: "k" })], 2)).toBeNull();
});
