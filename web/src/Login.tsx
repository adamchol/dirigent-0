// Signing in, in control's `oidc` auth: Google Identity Services' button gives an ID token,
// which control trades once for a sign-in of its own (control/src/signin.rs), kept in an
// `HttpOnly` cookie the page can't read. From that, control gives tokens of its own (good for
// minutes), which go to control as bearer tokens and to chlive as the credential; a new one is
// asked for before each expires. A page loaded again keeps the sign-in; signing out ends it.

import { useEffect, useMemo, useRef, useState } from "react";

import type { Who } from "./api.ts";

const GSI = "https://accounts.google.com/gsi/client";
/** A new token is asked for this long before the one in hand expires. */
const RENEW_BEFORE_MS = 60_000;

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
  renderButton(el: HTMLElement, options: { theme?: string; size?: string; shape?: string }): void;
  prompt(): void;
  disableAutoSelect(): void;
}

declare global {
  interface Window {
    google?: { accounts: { id: Gsi } };
  }
}

/** Google's button, in the page's theme. */
function buttonStyle() {
  const dark = document.documentElement.classList.contains("dark");
  return { theme: dark ? "filled_black" : "outline", size: "large", shape: "pill" };
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

/** A sign-in's answer: who, and control's token for its API and chlive. */
interface SignedIn {
  subject: string;
  token: string;
}

/** Control's sign-in: opened with Google's ID token, or (none) renewed from its cookie; none if
 * there's none to renew. */
async function signIn(idToken?: string): Promise<SignedIn | null> {
  const res = await fetch(idToken ? "/api/session" : "/api/session/token", {
    method: "POST",
    headers: idToken ? { authorization: `Bearer ${idToken}` } : {},
  });
  if (res.status === 401 && !idToken) return null;
  if (!res.ok) throw new Error(`${res.status}: ${(await res.text()) || res.statusText}`);
  return res.json();
}

/** Who signed in (`client_id` the web's Google client), once they have: by the sign-in their
 * cookie holds, or with Google's button, shown (`button`) when there's none; `checking` until
 * that's known. */
export function useGoogle(clientId: string): {
  who: Who | null;
  checking: boolean;
  button: React.RefObject<HTMLDivElement | null>;
  error: string;
  signOut: () => void;
} {
  const [signedIn, setSignedIn] = useState<SignedIn | null>(null);
  const [checking, setChecking] = useState(true);
  const [error, setError] = useState("");
  const button = useRef<HTMLDivElement | null>(null);
  const id = useRef<Gsi | null>(null);

  // the sign-in there is; else Google's button
  useEffect(() => {
    let gone = false;
    signIn()
      .catch(() => null)
      .then((s) => {
        if (gone) return;
        setSignedIn(s);
        setChecking(false);
      });
    return () => {
      gone = true;
    };
  }, []);

  useEffect(() => {
    if (checking || signedIn) return;
    let gone = false;
    gsi().then(
      (g) => {
        if (gone) return;
        id.current = g;
        g.initialize({
          client_id: clientId,
          callback: (r) =>
            signIn(r.credential).then(setSignedIn, (e) => setError(`Not signed in: ${(e as Error).message}`)),
          auto_select: true,
          use_fedcm_for_prompt: true,
        });
        if (button.current) g.renderButton(button.current, buttonStyle());
        g.prompt();
      },
      (e) => setError((e as Error).message),
    );
    return () => {
      gone = true;
    };
  }, [clientId, checking, signedIn]);

  // a new token before this one expires; the sign-in over, so is this
  useEffect(() => {
    if (!signedIn) return;
    const exp = (claims(signedIn.token).exp ?? 0) * 1000;
    const renew = setTimeout(
      () => signIn().then(setSignedIn, () => setSignedIn(null)),
      Math.max(0, exp - Date.now() - RENEW_BEFORE_MS),
    );
    return () => clearTimeout(renew);
  }, [signedIn]);

  // the same while the sign-in is
  const who = useMemo(() => (signedIn ? { user: signedIn.subject, token: signedIn.token } : null), [signedIn]);
  return {
    who,
    checking,
    button,
    error,
    signOut: () => {
      id.current?.disableAutoSelect();
      fetch("/api/session", { method: "DELETE" }).finally(() => setSignedIn(null));
    },
  };
}
