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
  | { kind: "turn_ended"; data: { reason: "completed" | "aborted" | "failed"; interrupt?: string } };

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
  | { t: "note"; key: string; tone: "dim" | "error"; text: string; detail?: string };

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
  const note = (key: string, tone: "dim" | "error", text: string, detail?: string) =>
    items.push({ t: "note", key, tone, text, detail });

  for (const row of rows) {
    let e: Event;
    try {
      e = parse(row);
    } catch {
      note(row.id, "error", `unreadable ${row.kind} event`, row.data);
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
        note(row.id, "dim", "model call cancelled");
        break;
      case "error":
        if (e.data.retry_in_ms !== undefined)
          note(row.id, "dim", `model error, retrying in ${(e.data.retry_in_ms / 1000).toFixed(1)}s`, e.data.message);
        else if (e.data.compact) note(row.id, "dim", "context too long, compacting", e.data.message);
        else note(row.id, "error", `model error: ${e.data.message}`);
        break;
      case "compacted":
        note(row.id, "dim", "context compacted", e.data.summary);
        break;
      case "compaction_failed":
        note(row.id, "error", `compaction failed: ${e.data.message}`);
        break;
      case "player_attached":
        note(row.id, "dim", `${row.author} attached (epoch ${e.data.epoch})`);
        break;
      case "player_detached":
        note(row.id, "dim", `${row.author} detached (epoch ${e.data.epoch})`);
        // a stopped player's open turn won't end
        openTurn = undefined;
        break;
      case "player_failed":
        note(row.id, "error", `player failed: ${e.data.error}`);
        openTurn = undefined;
        break;
      case "workspace_ready": {
        const from = e.data.source ? `${e.data.source.git.url}${e.data.source.git.ref ? `@${e.data.source.git.ref}` : ""}` : "empty";
        const head = e.data.head ? ` at ${e.data.head.slice(0, 12)}` : "";
        note(row.id, e.data.recreated ? "error" : "dim", `workspace ${e.data.recreated ? "recreated (changes lost)" : "ready"}: ${from}${head}`);
        break;
      }
      case "workspace_restored":
        note(row.id, "dim", `workspace restored from ${e.data.snapshot.slice(0, 12)}`);
        break;
      case "checkpoint":
        note(row.id, "dim", `checkpoint ${e.data.snapshot.slice(0, 12)}`);
        break;
      case "checkpoint_failed":
        note(row.id, "error", `checkpoint failed: ${e.data.error}`);
        break;
      default:
        note(row.id, "dim", (e as { kind: string }).kind);
    }
  }
  return { items, openTurn };
}

function taken(users: Map<string, { queued: boolean }>, input: string) {
  const u = users.get(input);
  if (u) u.queued = false;
}
