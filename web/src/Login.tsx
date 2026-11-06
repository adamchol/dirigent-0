// Signing in with Google, in control's `oidc` auth: Google Identity Services' button gives an
// ID token (a JWT, good for about an hour), which goes to control as a bearer token and to
// chlive as the credential. Shortly before it expires, Google is asked again, silently for a
// user who signed in before; if that fails, the button comes back.

import { useEffect, useMemo, useRef, useState } from "react";

import type { Who } from "./api.ts";

const GSI = "https://accounts.google.com/gsi/client";
/** A new token is asked for this long before the one in hand expires. */
const RENEW_BEFORE_MS = 5 * 60_000;

interface Credential {
  credential: string;
}

interface Gsi {
  initialize(config: {
    client_id: string;
    callback: (r: Credential) => void;
    auto_select?: boolean;
    use_fedcm_for_prompt?: boolean;
  }): void;
  renderButton(el: HTMLElement, options: { theme?: string; size?: string }): void;
  prompt(): void;
  disableAutoSelect(): void;
}

declare global {
  interface Window {
    google?: { accounts: { id: Gsi } };
  }
}

let loading: Promise<Gsi> | null = null;

function gsi(): Promise<Gsi> {
  loading ??= new Promise((resolve, reject) => {
    const s = document.createElement("script");
    s.src = GSI;
    s.async = true;
    s.onload = () => (window.google ? resolve(window.google.accounts.id) : reject(new Error("no Google sign-in")));
    s.onerror = () => reject(new Error("Google sign-in didn't load"));
    document.head.appendChild(s);
  });
  return loading;
}

/** An ID token's claims, read, not verified: control and chlive verify it; this only shows
 * who it says and when it expires. */
function claims(token: string): { email?: string; exp?: number } {
  try {
    const part = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    return JSON.parse(atob(part));
  } catch {
    return {};
  }
}

/** Who signed in with Google (`client_id` the web's), once they have; and the element for the
 * sign-in button, shown until then. */
export function useGoogle(clientId: string): {
  who: Who | null;
  button: React.RefObject<HTMLDivElement | null>;
  error: string;
  signOut: () => void;
} {
  const [token, setToken] = useState<string | null>(null);
  const [error, setError] = useState("");
  const button = useRef<HTMLDivElement | null>(null);
  const id = useRef<Gsi | null>(null);

  useEffect(() => {
    let gone = false;
    gsi().then(
      (g) => {
        if (gone) return;
        id.current = g;
        g.initialize({
          client_id: clientId,
          callback: (r) => setToken(r.credential),
          auto_select: true,
          use_fedcm_for_prompt: true,
        });
        if (button.current) g.renderButton(button.current, { theme: "outline", size: "large" });
        g.prompt();
      },
      (e) => setError((e as Error).message),
    );
    return () => {
      gone = true;
    };
  }, [clientId]);

  // renew it before it expires; expired, it's gone
  useEffect(() => {
    if (!token) {
      if (button.current && id.current) id.current.renderButton(button.current, { theme: "outline", size: "large" });
      return;
    }
    const exp = (claims(token).exp ?? 0) * 1000;
    const renew = setTimeout(() => id.current?.prompt(), Math.max(0, exp - Date.now() - RENEW_BEFORE_MS));
    const expire = setTimeout(() => setToken(null), Math.max(0, exp - Date.now()));
    return () => {
      clearTimeout(renew);
      clearTimeout(expire);
    };
  }, [token]);

  // the same while the token is
  const who = useMemo(() => {
    const email = token ? claims(token).email : undefined;
    return token && email ? { user: `user:${email}`, idToken: token } : null;
  }, [token]);
  return {
    who,
    button,
    error,
    signOut: () => {
      id.current?.disableAutoSelect();
      setToken(null);
    },
  };
}
