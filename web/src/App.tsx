// The page: the sessions control has (left), and the one picked (right, its id in the URL's
// hash), whose log is read live through chlive as the user chosen at the top.

import { useEffect, useState } from "react";
import { LiveClient, defaultUrl } from "@chlive/client";
import { LiveClientProvider } from "@chlive/client/react";

import { Session } from "./Session.tsx";
import { Sessions } from "./Sessions.tsx";

/** A connection to chlive as `user`, closed when the user changes. */
function useClient(user: string): LiveClient | null {
  const [client, setClient] = useState<LiveClient | null>(null);
  useEffect(() => {
    const c = new LiveClient({ url: defaultUrl(), user, cache: false });
    setClient(c);
    return () => c.close();
  }, [user]);
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
  const [user, setUser] = useState(() => remembered("dirigent.user", "user:dev"));
  const [draft, setDraft] = useState(user);
  const [selected, select] = useHash();
  const client = useClient(user);

  function changeUser(e: React.FormEvent) {
    e.preventDefault();
    const u = draft.trim();
    if (!u) return;
    setUser(u);
    try {
      localStorage.setItem("dirigent.user", u);
    } catch {
      // a private window: not remembered
    }
  }

  return (
    <div className="app">
      <header>
        <h1>dirigent</h1>
        <form className="user" onSubmit={changeUser}>
          <label htmlFor="user">as</label>
          <input id="user" value={draft} onChange={(e) => setDraft(e.target.value)} spellCheck={false} />
        </form>
      </header>
      <Sessions selected={selected} select={select} user={user} />
      <main>
        {!selected ? (
          <p className="empty">Pick a session.</p>
        ) : client ? (
          <LiveClientProvider client={client}>
            <Session key={`${user}/${selected}`} id={selected} client={client} />
          </LiveClientProvider>
        ) : null}
      </main>
    </div>
  );
}
