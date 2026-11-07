// A session's log as the UI reads it: rows of `session_events` (protocol/src/rows.rs), their
// events (protocol/src/lib.rs, `Event`), and the timeline they make.

/** One row of the log, as `LOG_QUERY` selects it. */
export interface EventRow {
  session_id: string;
  offset: string | number;
  partition: string | number;
  ts: string | Date;
  id: string;
  author: string;
  kind: string;
  /** The event's fields as JSON; empty for none. */
  data: string;
}

/** `LOG_QUERY` (protocol/src/rows.rs): the log without its streaming deltas, in order. */
export const LOG_QUERY =
  "select session_id, offset, partition, ts, id, author, kind, data " +
  "from session_events where session_id = {session:UUID} and kind != 'model_delta' " +
  "order by offset";

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; text: string }
  | { type: "tool_call"; id: string; name: string; input: unknown }
  | { type: "tool_result"; call_id: string; content: string; is_error?: boolean };

export interface Usage {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

export type Source = { git: { url: string; ref?: string } };

export type Event =
  | { kind: "player_attached"; data: { epoch: number } }
  | { kind: "player_detached"; data: { epoch: number } }
  | { kind: "player_failed"; data: { epoch: number; error: string } }
  | { kind: "workspace_ready"; data: { source?: Source; head?: string; recreated?: boolean } }
  | { kind: "workspace_restored"; data: { snapshot: string } }
  | { kind: "checkpoint"; data: { turn: string; snapshot: string } }
  | { kind: "checkpoint_failed"; data: { turn: string; error: string } }
  | { kind: "user_message"; data: { text: string; via?: string } }
  | {
      kind: "session_update";
      data: {
        session: string;
        offset: number;
        update:
          | { what: "turn_ended"; reason: "completed" | "aborted" | "failed" }
          | { what: "player_failed"; error: string }
          | { what: "player_detached" };
        reply?: string;
      };
    }
  | { kind: "user_interrupt"; data: { turn: string } }
  | { kind: "turn_started"; data: { input: string } }
  | { kind: "steered"; data: { input: string } }
  | {
      kind: "assistant_message";
      data: { content: ContentBlock[]; stop_reason: { reason: string; raw?: string }; usage: Usage };
    }
  | { kind: "model_cancelled"; data?: undefined }
  | { kind: "error"; data: { message: string; retry_in_ms?: number; compact?: boolean } }
  | { kind: "compacted"; data: { summary: string; first_kept: string; usage: Usage } }
  | { kind: "compaction_failed"; data: { message: string } }
  | {
      kind: "tool_result";
      data: { call_id: string; name: string; content: string; is_error: boolean; meta?: unknown };
    }
  | { kind: "turn_ended"; data: { reason: "completed" | "aborted" | "failed"; interrupt?: string } }
  | { kind: "titled"; data: Named };

/** What a session is called and is about, by its player or (`hold`ing it) its owner. */
export interface Named {
  title: string;
  description: string;
  hold?: boolean;
}

/** One thing the timeline shows. `key` is stable across updates (row ids, call ids). */
export type Item =
  /** `queued` until a turn takes it; `via`, the dirigent session that wrote it as its author. */
  | { t: "user"; key: string; author: string; ts: string | Date; text: string; queued: boolean; via?: string }
  /** What happened in a session a dirigent directs: an input of its, as a message is. */
  | { t: "update"; key: string; session: string; what: string; reply?: string; queued: boolean }
  | { t: "text"; key: string; text: string }
  | { t: "thinking"; key: string; text: string }
  /** A tool call with its result, once there is one. */
  | { t: "tool"; key: string; name: string; input: unknown; result?: { content: string; isError: boolean } }
  | { t: "turn"; key: string; reason: string; interrupted: boolean; usage: Usage }
  /** Anything else: `dim` for the player's own doings, `error` for failures. */
  | { t: "note"; key: string; kind: string; tone: "dim" | "error"; text: string; detail?: string };

export interface Timeline {
  items: Item[];
  /** The turn running (its `turn_started`'s id), if one is. */
  openTurn?: string;
}

const NO_USAGE: Usage = { input: 0, output: 0, cache_read: 0, cache_write: 0 };

function add(a: Usage, b: Usage): Usage {
  return {
    input: a.input + b.input,
    output: a.output + b.output,
    cache_read: a.cache_read + b.cache_read,
    cache_write: a.cache_write + b.cache_write,
  };
}

export function parse(row: EventRow): Event {
  return { kind: row.kind, data: row.data ? JSON.parse(row.data) : undefined } as Event;
}

/** The log's rows, in order, as a timeline. */
export function timeline(rows: readonly EventRow[]): Timeline {
  const items: Item[] = [];
  const users = new Map<string, { queued: boolean }>();
  const tools = new Map<string, Extract<Item, { t: "tool" }>>();
  let openTurn: string | undefined;
  let usage = NO_USAGE;
  const note = (row: EventRow, tone: "dim" | "error", text: string, detail?: string) =>
    items.push({ t: "note", key: row.id, kind: row.kind, tone, text, detail });

  for (const row of rows) {
    let e: Event;
    try {
      e = parse(row);
    } catch {
      note(row, "error", `unreadable ${row.kind} event`, row.data);
      continue;
    }
    switch (e.kind) {
      case "user_message": {
        const user: Extract<Item, { t: "user" }> = {
          t: "user",
          key: row.id,
          author: row.author,
          ts: row.ts,
          text: e.data.text,
          queued: true,
          ...(e.data.via ? { via: e.data.via } : {}),
        };
        users.set(row.id, user);
        items.push(user);
        break;
      }
      case "session_update": {
        const u = e.data.update;
        const what =
          u.what === "turn_ended"
            ? `turn ${u.reason}`
            : u.what === "player_failed"
              ? `player failed: ${u.error}`
              : "stopped";
        const update: Extract<Item, { t: "update" }> = {
          t: "update",
          key: row.id,
          session: e.data.session,
          what,
          queued: true,
          ...(e.data.reply ? { reply: e.data.reply } : {}),
        };
        users.set(row.id, update);
        items.push(update);
        break;
      }
      case "turn_started":
        openTurn = row.id;
        usage = NO_USAGE;
        taken(users, e.data.input);
        break;
      case "steered":
        taken(users, e.data.input);
        break;
      case "user_interrupt":
        break;
      case "assistant_message":
        usage = add(usage, e.data.usage);
        e.data.content.forEach((b, i) => {
          const key = `${row.id}:${i}`;
          if (b.type === "text" && b.text) items.push({ t: "text", key, text: b.text });
          else if (b.type === "thinking" && b.text) items.push({ t: "thinking", key, text: b.text });
          else if (b.type === "tool_call") {
            const tool = { t: "tool", key: b.id, name: b.name, input: b.input } as Extract<Item, { t: "tool" }>;
            tools.set(b.id, tool);
            items.push(tool);
          }
        });
        break;
      case "tool_result": {
        const result = { content: e.data.content, isError: e.data.is_error };
        const tool = tools.get(e.data.call_id);
        if (tool) tool.result = result;
        else items.push({ t: "tool", key: e.data.call_id, name: e.data.name, input: undefined, result });
        break;
      }
      case "turn_ended":
        items.push({
          t: "turn",
          key: row.id,
          reason: e.data.reason,
          interrupted: e.data.interrupt !== undefined,
          usage,
        });
        openTurn = undefined;
        break;
      case "model_cancelled":
        note(row, "dim", "model call cancelled");
        break;
      case "error":
        if (e.data.retry_in_ms !== undefined)
          note(row, "dim", `model error, retrying in ${(e.data.retry_in_ms / 1000).toFixed(1)}s`, e.data.message);
        else if (e.data.compact) note(row, "dim", "context too long, compacting", e.data.message);
        else note(row, "error", `model error: ${e.data.message}`);
        break;
      case "compacted":
        note(row, "dim", "context compacted", e.data.summary);
        break;
      case "compaction_failed":
        note(row, "error", `compaction failed: ${e.data.message}`);
        break;
      case "player_attached":
        note(row, "dim", `${subject(row.author)} attached (epoch ${e.data.epoch})`);
        break;
      case "player_detached":
        note(row, "dim", `${subject(row.author)} detached (epoch ${e.data.epoch})`);
        // a stopped player's open turn won't end
        openTurn = undefined;
        break;
      case "player_failed":
        note(row, "error", `player failed: ${e.data.error}`);
        openTurn = undefined;
        break;
      case "workspace_ready": {
        const from = e.data.source ? `${e.data.source.git.url}${e.data.source.git.ref ? `@${e.data.source.git.ref}` : ""}` : "empty";
        const head = e.data.head ? ` at ${e.data.head.slice(0, 12)}` : "";
        note(row, e.data.recreated ? "error" : "dim", `workspace ${e.data.recreated ? "recreated (changes lost)" : "ready"}: ${from}${head}`);
        break;
      }
      case "workspace_restored":
        note(row, "dim", `workspace restored from ${e.data.snapshot.slice(0, 12)}`);
        break;
      case "checkpoint":
        note(row, "dim", `checkpoint ${e.data.snapshot.slice(0, 12)}`);
        break;
      case "checkpoint_failed":
        note(row, "error", `checkpoint failed: ${e.data.error}`);
        break;
      case "titled":
        note(row, "dim", `${e.data.hold ? "renamed" : "titled"}: ${e.data.title}`, e.data.description);
        break;
      default:
        note(row, "dim", (e as { kind: string }).kind);
    }
  }
  return { items, openTurn };
}

function taken(users: Map<string, { queued: boolean }>, input: string) {
  const u = users.get(input);
  if (u) u.queued = false;
}

/** A subject, short: a player's id cut to its first part (`player:7f6bc753`). */
function subject(s: string): string {
  const m = /^(player:[0-9a-f]{8})-[0-9a-f-]+$/.exec(s);
  return m ? m[1]! : s;
}

/** The model call running's streaming pieces (`model_delta`, which `LOG_QUERY` leaves out): the
 * session's deltas after `after`, the offset of the log's last row before that call began
 * (`callStart`). */
export const DELTAS_QUERY =
  "select offset, id, data from session_events " +
  "where session_id = {session:UUID} and kind = 'model_delta' and offset > {after:UInt64} " +
  "order by offset";

export interface DeltaRow {
  offset: string | number;
  id: string;
  /** `{"delta": …}`: protocol/src/lib.rs, `Delta`. */
  data: string;
}

/** What ends a model call, or starts one: the deltas after the last of these are the running
 * call's (a retried call's start over after its error). */
const CALL_BOUNDARIES = new Set([
  "turn_started",
  "assistant_message",
  "model_cancelled",
  "error",
  "turn_ended",
  "player_attached",
]);

/** The offset the running model call's deltas come after. */
export function callStart(rows: readonly EventRow[]): number {
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i]!;
    if (CALL_BOUNDARIES.has(row.kind)) return Number(row.offset);
  }
  return 0;
}

/** A block of the reply being written, as its deltas have it so far. */
export type Streamed =
  | { t: "text"; key: string; text: string }
  | { t: "thinking"; key: string; text: string }
  /** `input` as far as its arguments parse yet. */
  | { t: "tool"; key: string; name: string; input: unknown };

type Delta =
  | { type: "block_start"; index: number; block: { type: string; text?: string; name?: string; input?: unknown } }
  | { type: "text" | "thinking"; index: number; text: string }
  | { type: "tool_call_args"; index: number; json: string }
  | { type: string; index?: number };

/** The deltas, in order, as the blocks they make. */
export function streamed(rows: readonly DeltaRow[]): Streamed[] {
  const blocks: (Streamed | undefined)[] = [];
  for (const row of rows) {
    let d: Delta;
    try {
      d = JSON.parse(row.data).delta;
    } catch {
      continue;
    }
    const key = `stream:${d.index}`;
    if (d.type === "block_start" && "block" in d) {
      const b = d.block;
      blocks[d.index] =
        b.type === "text"
          ? { t: "text", key, text: b.text ?? "" }
          : b.type === "thinking"
            ? { t: "thinking", key, text: b.text ?? "" }
            : b.type === "tool_call"
              ? { t: "tool", key, name: b.name ?? "", input: b.input }
              : undefined;
    } else if ((d.type === "text" || d.type === "thinking") && "text" in d) {
      const b = blocks[d.index];
      if (b && b.t === d.type) b.text += d.text;
    } else if (d.type === "tool_call_args" && "json" in d) {
      const b = blocks[d.index];
      if (b?.t === "tool") {
        try {
          b.input = JSON.parse(d.json);
        } catch {
          // not whole yet: the last that parsed stays
        }
      }
    }
  }
  return blocks.filter((b): b is Streamed => b !== undefined);
}

/** Each session's latest event (its streaming deltas aside), of those the user may read: how
 * recently it was active. */
// a rename isn't activity: it doesn't move the session up the list
export const ACTIVITY_QUERY =
  "select session_id, max(ts) as last from session_events where kind not in ('model_delta', 'titled') group by session_id";

/** Every title the sessions had: a few each (chlive has no `argMax` to keep only the latest). */
export const TITLES_QUERY = "select session_id, offset, data from session_events where kind = 'titled'";

export interface TitleRow {
  session_id: string;
  offset: number;
  data: string;
}

/** Each session's latest title (a session's rows share a partition: the last has the highest
 * offset), leaving out any that doesn't read. */
export function titles(rows: TitleRow[]): Map<string, Named> {
  const latest = new Map<string, { offset: number; named: Named }>();
  for (const r of rows) {
    try {
      const named = JSON.parse(r.data) as Named;
      const seen = latest.get(r.session_id);
      if (named.title && (!seen || Number(r.offset) > seen.offset)) latest.set(r.session_id, { offset: Number(r.offset), named });
    } catch {
      // not a title
    }
  }
  return new Map([...latest].map(([id, { named }]) => [id, named]));
}

export interface ActivityRow {
  session_id: string;
  last: string | Date;
}

/** A row's timestamp (the client's parsed one, or ClickHouse's DateTime64 text, UTC) in
 * milliseconds since the Unix epoch; NaN if it isn't one. */
export function millis(ts: string | Date): number {
  return ts instanceof Date ? ts.getTime() : Date.parse(ts.includes("T") ? ts : `${ts.replace(" ", "T")}Z`);
}

/** How much of the model's context the session uses: its last call's input (cached or not) and
 * output, which the next call carries; 0 before it has one. */
export function contextSize(rows: readonly EventRow[]): number {
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i]!;
    if (row.kind !== "assistant_message") continue;
    try {
      const u: Usage = JSON.parse(row.data).usage;
      return u.input + u.cache_read + u.cache_write + u.output;
    } catch {
      return 0;
    }
  }
  return 0;
}
