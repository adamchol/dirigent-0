// The page: the sessions control has (left), and the one picked (right, its id in the URL's
// hash), whose log is read live through chlive. Who looks depends on control's auth: in
// `declared` (dev), the user chosen at the top; in `oidc`, whoever signed in with Google.

import { useEffect, useMemo, useRef, useState } from "react";
import { LiveClient, defaultUrl } from "@chlive/client";
import { LiveClientProvider } from "@chlive/client/react";

import * as api from "./api.ts";
import type { Who } from "./api.ts";
import { useGoogle } from "./Login.tsx";
import { Session } from "./Session.tsx";
import { Sessions, useSessions } from "./Sessions.tsx";

/** A connection to chlive as `who`, closed when they change (not when their token's renewed:
 * the client asks for the latest one). */
function useClient(who: Who): LiveClient | null {
  const latest = useRef(who);
  latest.current = who;
  const [client, setClient] = useState<LiveClient | null>(null);
  useEffect(() => {
    const c = new LiveClient({
      url: defaultUrl(),
      user: who.user,
      token: () => api.token(latest.current),
      cache: false,
    });
    setClient(c);
    return () => c.close();
  }, [who.user]);
  return client;
}

function useHash(): [string, (h: string) => void] {
  const read = () => decodeURIComponent(location.hash.slice(1));
  const [hash, setHash] = useState(read);
  useEffect(() => {
    const changed = () => setHash(read());
    addEventListener("hashchange", changed);
    return () => removeEventListener("hashchange", changed);
  }, []);
  return [hash, (h) => (location.hash = h)];
}

function remembered(key: string, fallback: string): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}

export function App() {
  const [auth, setAuth] = useState<api.Auth | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    api.auth().then(setAuth, (e) => setError((e as Error).message));
  }, []);
  if (error) return <p className="empty error">control: {error}</p>;
  if (!auth) return null;
  return auth.mode === "oidc" ? <SignedIn clientId={auth.google_client_id} /> : <Declared />;
}

/** `declared` auth: whoever the box at the top says. */
function Declared() {
  const [user, setUser] = useState(() => remembered("dirigent.user", "user:dev"));
  const [draft, setDraft] = useState(user);
  const who = useMemo(() => ({ user }), [user]);

  function become(to: string) {
    const u = to.trim();
    if (!u) return;
    setUser(u);
    setDraft(u);
    try {
      localStorage.setItem("dirigent.user", u);
    } catch {
      // a private window: not remembered
    }
  }

  const top = (
    <form
      className="user"
      onSubmit={(e) => {
        e.preventDefault();
        become(draft);
      }}
    >
      <label htmlFor="user">as</label>
      <input id="user" value={draft} onChange={(e) => setDraft(e.target.value)} spellCheck={false} />
    </form>
  );
  return <Page who={who} top={top} become={become} />;
}

/** `oidc` auth: whoever signed in with Google. */
function SignedIn(props: { clientId: string }) {
  const google = useGoogle(props.clientId);
  if (!google.who)
    return (
      <div className="app">
        <header>
          <h1>dirigent</h1>
        </header>
        <main className="login">
          <div ref={google.button} />
          {google.error && <div className="error">{google.error}</div>}
        </main>
      </div>
    );
  const top = (
    <div className="user">
      <span>{google.who.user}</span>
      <button type="button" onClick={google.signOut}>
        Sign out
      </button>
    </div>
  );
  return <Page who={google.who} top={top} />;
}

function Page(props: { who: Who; top: React.ReactNode; become?: (user: string) => void }) {
  const [selected, select] = useHash();
  const client = useClient(props.who);
  const sessions = useSessions(props.who);
  const info = sessions.list.find((s) => s.id === selected);

  return (
    <div className="app">
      <header>
        <h1>dirigent</h1>
        {props.top}
      </header>
      <Sessions sessions={sessions} selected={selected} select={select} who={props.who} />
      <main>
        {!selected ? (
          <p className="empty">Pick a session.</p>
        ) : client ? (
          <LiveClientProvider client={client}>
            <Session
              key={`${props.who.user}/${selected}`}
              id={selected}
              client={client}
              who={props.who}
              info={info}
              listed={sessions.listed}
              become={props.become}
              changed={sessions.refresh}
            />
          </LiveClientProvider>
        ) : null}
      </main>
    </div>
  );
}
