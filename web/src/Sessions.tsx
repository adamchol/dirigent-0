// The sessions the user owns or views, polled from control (it has no push), with their state,
// and starting, stopping and making them.

import { useCallback, useEffect, useState } from "react";

import * as api from "./api.ts";
import type { Effort, Part, SessionInfo } from "./api.ts";

const POLL_MS = 2000;

export interface Polled {
  list: SessionInfo[];
  /** Whether a list came. */
  listed: boolean;
  error: string;
  setError: (e: string) => void;
  refresh: () => void;
}

/** `who`'s sessions, polled. */
export function useSessions(who: api.Who): Polled {
  const [list, setList] = useState<SessionInfo[]>([]);
  const [listed, setListed] = useState(false);
  const [error, setError] = useState("");

  const refresh = useCallback(async () => {
    try {
      setList(await api.sessions(who));
      setListed(true);
      setError("");
    } catch (e) {
      setError(`control: ${(e as Error).message}`);
    }
  }, [who]);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, POLL_MS);
    return () => clearInterval(t);
  }, [refresh]);

  return { list, listed, error, setError, refresh };
}

export function Sessions(props: { sessions: Polled; selected: string; select: (id: string) => void; who: api.Who }) {
  const { list, error, setError, refresh } = props.sessions;

  /** Run `f`, then show what it changed (or why it failed). */
  const act = (f: () => Promise<void>) => async () => {
    try {
      await f();
      setError("");
    } catch (e) {
      setError((e as Error).message);
    }
    refresh();
  };

  return (
    <nav className="sessions">
      <NewSession
        who={props.who}
        made={(id) => {
          props.select(id);
          refresh();
        }}
        failed={setError}
      />
      {error && <div className="error">{error}</div>}
      <ul>
        {list.map((s) => {
          const st = api.state(s);
          const owner = s.role === "owner";
          return (
            <li key={s.id} className={s.id === props.selected ? "selected" : undefined}>
              <button type="button" className="pick" onClick={() => props.select(s.id)} title={s.id}>
                <span className={`state ${st}`}>{st}</span>
                <code>{s.id.slice(0, 8)}</code>
                {api.part(s) === "dirigent" && <span className="part">dirigent</span>}
                <span className="meta">
                  #{s.epoch} · {s.spec.model}
                  {s.spec.effort ? ` · ${s.spec.effort}` : ""}
                </span>
                {s.spec.source && <span className="meta source">{source(s)}</span>}
                {!owner && <span className="meta users other">{s.owner}'s · viewing</span>}
              </button>
              {owner && (
                <span className="actions">
                  {st === "idle" ? (
                    <button type="button" onClick={act(() => api.start(props.who, s.id))}>
                      Start
                    </button>
                  ) : (
                    <>
                      <button
                        type="button"
                        onClick={act(() => api.stop(props.who, s.id, false))}
                        title="finish the turn, then detach"
                      >
                        Stop
                      </button>
                      <button
                        type="button"
                        onClick={act(() => api.stop(props.who, s.id, true))}
                        title="cancel the turn and detach"
                      >
                        now
                      </button>
                    </>
                  )}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </nav>
  );
}

function source(s: SessionInfo): string {
  const git = s.spec.source?.git;
  return git ? `${git.url.replace(/^https?:\/\//, "")}${git.ref ? `@${git.ref}` : ""}` : "";
}

function NewSession(props: { who: api.Who; made: (id: string) => void; failed: (e: string) => void }) {
  const [open, setOpen] = useState(false);
  const [part, setPart] = useState<Part>("coder");
  const [git, setGit] = useState("");
  const [ref, setRef] = useState("");
  const [model, setModel] = useState("claude-sonnet-5");
  const [effort, setEffort] = useState<Effort | "">("medium");
  const [start, setStart] = useState(true);

  if (!open)
    return (
      <button type="button" className="new" onClick={() => setOpen(true)}>
        New session
      </button>
    );

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const id = crypto.randomUUID();
    const source =
      part === "coder" && git.trim() ? { git: { url: git.trim(), ...(ref.trim() ? { ref: ref.trim() } : {}) } } : null;
    try {
      await api.put(props.who, id, { model, effort: effort || null, source, part }, start);
      setOpen(false);
      props.made(id);
    } catch (err) {
      props.failed((err as Error).message);
    }
  }

  return (
    <form className="new-session" onSubmit={submit}>
      <select value={part} onChange={(e) => setPart(e.target.value as Part)}>
        <option value="coder">coder: works in a repository</option>
        <option value="dirigent">dirigent: directs your sessions</option>
      </select>
      {part === "coder" && (
        <>
          <input placeholder="git URL (empty: no files)" value={git} onChange={(e) => setGit(e.target.value)} />
          <input placeholder="ref (default branch)" value={ref} onChange={(e) => setRef(e.target.value)} />
        </>
      )}
      <input placeholder="model" value={model} onChange={(e) => setModel(e.target.value)} required />
      <select value={effort} onChange={(e) => setEffort(e.target.value as Effort | "")}>
        <option value="">no thinking</option>
        <option value="low">low</option>
        <option value="medium">medium</option>
        <option value="high">high</option>
        <option value="max">max</option>
      </select>
      <label>
        <input type="checkbox" checked={start} onChange={(e) => setStart(e.target.checked)} /> start
      </label>
      <p className="meta">for {props.who.user}</p>
      <span className="actions">
        <button type="submit">Make</button>
        <button type="button" onClick={() => setOpen(false)}>
          Cancel
        </button>
      </span>
    </form>
  );
}
