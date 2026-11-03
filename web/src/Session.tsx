// A session's log, live through chlive as the client's user, as a timeline; and what that user
// writes into it: messages, and interrupting the running turn.

import { useEffect, useMemo, useRef, useState } from "react";
import type { LiveClient } from "@chlive/client";
import { useLiveQuery } from "@chlive/client/react";

import { LOG_QUERY, timeline, type EventRow, type Item, type Usage } from "./events.ts";

export function Session(props: { id: string; client: LiveClient }) {
  const log = useLiveQuery<EventRow & Record<string, unknown>>(LOG_QUERY, { params: { session: props.id } });
  const { items, openTurn } = useMemo(() => timeline(log.rows), [log.rows]);

  // keep the newest event in view, unless the user scrolled up
  const list = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  useEffect(() => {
    if (pinned.current) list.current?.scrollTo({ top: list.current.scrollHeight });
  }, [items]);

  const [draft, setDraft] = useState("");
  const [error, setError] = useState("");

  async function write(kind: string, data: unknown) {
    try {
      await props.client.insert("session_events", {
        session_id: props.id,
        id: crypto.randomUUID(),
        kind,
        data: JSON.stringify(data),
      });
      setError("");
    } catch (e) {
      setError((e as Error).message);
    }
  }

  function send(e: React.FormEvent) {
    e.preventDefault();
    const text = draft.trim();
    if (!text) return;
    setDraft("");
    write("user_message", { text });
  }

  return (
    <section className="session">
      <div className="session-top">
        <span className={`dot ${log.status}`} title={log.error?.message ?? log.status} />
        <code>{props.id}</code>
        {openTurn && <span className="running">turn running</span>}
      </div>
      {log.status === "error" ? (
        <div className="log denied">No log: {log.error?.message}</div>
      ) : (
        <div
          className="log"
          ref={list}
          onScroll={(e) => {
            const el = e.currentTarget;
            pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
          }}
        >
          {log.status === "live" && items.length === 0 && <p className="empty">Nothing yet.</p>}
          {items.map((item) => (
            <Entry key={item.key} item={item} />
          ))}
        </div>
      )}
      {error && <div className="error">{error}</div>}
      <form className="compose" onSubmit={send}>
        <textarea
          value={draft}
          rows={2}
          placeholder="Message"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) send(e);
          }}
        />
        <button type="submit">Send</button>
        {openTurn && (
          <button type="button" className="interrupt" onClick={() => write("user_interrupt", { turn: openTurn })}>
            Interrupt
          </button>
        )}
      </form>
    </section>
  );
}

function Entry({ item }: { item: Item }) {
  switch (item.t) {
    case "user":
      return (
        <div className={`entry user${item.queued ? " queued" : ""}`}>
          <div className="who">
            {item.author} <span className="meta">{time(item.ts)}</span>
            {item.queued && <span className="meta"> · queued</span>}
          </div>
          <div className="text">{item.text}</div>
        </div>
      );
    case "text":
      return <div className="entry assistant text">{item.text}</div>;
    case "thinking":
      return (
        <details className="entry thinking">
          <summary>thinking</summary>
          <div className="text">{item.text}</div>
        </details>
      );
    case "tool":
      return (
        <details className={`entry tool${item.result?.isError ? " failed" : ""}${item.result ? "" : " waiting"}`}>
          <summary>
            <code>{item.name}</code> <span className="meta">{brief(item.input)}</span>
            {!item.result && <span className="meta"> · running</span>}
          </summary>
          {item.input !== undefined && <pre>{JSON.stringify(item.input, null, 2)}</pre>}
          {item.result && <pre className="result">{item.result.content}</pre>}
        </details>
      );
    case "turn":
      return (
        <div className={`entry turn ${item.reason}`}>
          turn {item.interrupted ? "interrupted" : item.reason} <span className="meta">{usage(item.usage)}</span>
        </div>
      );
    case "note":
      return item.detail ? (
        <details className={`entry note ${item.tone}`}>
          <summary>{item.text}</summary>
          <div className="text">{item.detail}</div>
        </details>
      ) : (
        <div className={`entry note ${item.tone}`}>{item.text}</div>
      );
  }
}

/** A tool call's input in a line: its first string field, cut short. */
function brief(input: unknown): string {
  if (!input || typeof input !== "object") return "";
  const first = Object.values(input).find((v) => typeof v === "string") as string | undefined;
  if (!first) return "";
  const line = first.split("\n")[0];
  return line.length > 80 ? `${line.slice(0, 80)}…` : line;
}

function usage(u: Usage): string {
  return `${u.input + u.cache_read + u.cache_write} in (${u.cache_read} cached) · ${u.output} out`;
}

/** A row's timestamp (the client's parsed one, or ClickHouse's DateTime64 text, UTC) as local
 * time. */
function time(ts: string | Date): string {
  const d = ts instanceof Date ? ts : new Date(ts.includes("T") ? ts : `${ts.replace(" ", "T")}Z`);
  return isNaN(d.getTime()) ? String(ts) : d.toLocaleTimeString();
}
