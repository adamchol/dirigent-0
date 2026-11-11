// Private GitHub repositories: what gives a user's sessions access to them. Their own
// GITHUB_TOKEN secret, or, when control has a GitHub App, the installations of it they connected,
// whose tokens (one per session's repository, renewed hourly) their sessions get. The new-session
// dialog says, before a session's made, whether its repository can be read and how.

import { useCallback, useEffect, useState } from "react";
import { CheckIcon, CircleAlertIcon, GitPullRequestIcon, KeyRoundIcon, PlugIcon, RefreshCwIcon, Trash2Icon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import * as api from "./api.ts";
import type { Access, GithubLink, Who } from "./api.ts";

/** Send the browser to GitHub to connect it (installing the App, or for `existing`
 * installations, authorizing it); it comes back to the page with `?github=…`. */
export async function connectGithub(who: Who, existing = false) {
  try {
    window.location.assign(await api.connectGithub(who, existing));
  } catch (err) {
    toast.error("GitHub can't be connected", { description: (err as Error).message });
  }
}

/** What GitHub sent the browser back with, said once and taken off the URL. */
export function useGithubReturn(connected: () => void) {
  useEffect(() => {
    const url = new URL(window.location.href);
    const ok = url.searchParams.get("github") === "connected";
    const error = url.searchParams.get("github_error");
    if (!ok && error === null) return;
    if (ok) {
      toast.success("GitHub connected");
      connected();
    } else toast.error("GitHub isn't connected", { description: error });
    url.searchParams.delete("github");
    url.searchParams.delete("github_error");
    window.history.replaceState(null, "", url);
  }, [connected]);
}

/** How the user's sessions reach private repositories: their GitHub App connection, if control
 * has an App, and their token. */
export function GithubDialog(props: { open: boolean; who: Who; addToken: () => void; close: () => void }) {
  const [link, setLink] = useState<GithubLink | null>(null);
  const load = useCallback(() => {
    api.githubLink(props.who).then(setLink, (e: Error) => toast.error("GitHub not read", { description: e.message }));
  }, [props.who]);
  useEffect(() => {
    if (!props.open) return;
    setLink(null);
    load();
  }, [props.open, load]);

  async function disconnect() {
    try {
      await api.unlinkGithub(props.who);
      load();
    } catch (err) {
      toast.error("Not disconnected", { description: (err as Error).message });
    }
  }

  return (
    <Dialog open={props.open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>GitHub</DialogTitle>
          <DialogDescription>
            Sessions clone public repositories as they are. For private ones, and for pushing, they need access:{" "}
            {link?.app ? "connect GitHub, or give a token of your own." : "give a token of your own."}
          </DialogDescription>
        </DialogHeader>
        {link === null ? (
          <div className="flex justify-center p-3">
            <Spinner />
          </div>
        ) : (
          <div className="flex flex-col gap-4 text-[13px]">
            {link.app && (
              <section className="flex flex-col gap-2 rounded-md border p-3">
                <div className="flex items-center gap-2 font-medium">
                  <GitPullRequestIcon className="size-4" /> dirigent's GitHub App
                </div>
                <p className="text-muted-foreground">
                  {link.accounts.length > 0
                    ? `Connected: ${link.accounts.join(", ")}. Sessions on the repositories you gave it get a token for theirs alone.`
                    : "Install it on your account (or an organization) and pick the repositories it may use."}
                </p>
                <div className="flex flex-wrap gap-2">
                  <Button size="sm" onClick={() => connectGithub(props.who)}>
                    <PlugIcon /> {link.accounts.length > 0 ? "Add an account or repositories" : "Connect GitHub"}
                  </Button>
                  <Button size="sm" variant="outline" onClick={() => connectGithub(props.who, true)}>
                    <RefreshCwIcon /> Find installations
                  </Button>
                  {link.accounts.length > 0 && (
                    <Button size="sm" variant="ghost" className="text-destructive" onClick={disconnect}>
                      <Trash2Icon /> Disconnect
                    </Button>
                  )}
                </div>
              </section>
            )}
            <section className="flex flex-col gap-2 rounded-md border p-3">
              <div className="flex items-center gap-2 font-medium">
                <KeyRoundIcon className="size-4" /> Your own token
              </div>
              <p className="text-muted-foreground">
                A <code className="font-mono text-[12px]">GITHUB_TOKEN</code> secret: git and <code className="font-mono text-[12px]">gh</code> use it
                in your sessions, and it wins over the App. Make a{" "}
                <a className="text-primary-text hover:underline" href={TOKEN_PAGE} target="_blank" rel="noreferrer">
                  fine-grained token
                </a>{" "}
                with Contents read and write on the repositories.
              </p>
              <div>
                <Button size="sm" variant="outline" onClick={props.addToken}>
                  <KeyRoundIcon /> Add GITHUB_TOKEN
                </Button>
              </div>
            </section>
          </div>
        )}
        <DialogFooter>
          <DialogClose render={<Button type="button" variant="outline" />}>Done</DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** GitHub's page for a new fine-grained token. */
export const TOKEN_PAGE = "https://github.com/settings/personal-access-tokens/new";

/** Whether the user's sessions can read the repository at `url`, said under the new session's
 * Repository field: checked with control once the user stops typing a github.com URL. */
export function RepoAccess(props: { who: Who; url: string; addToken: () => void; connect: () => void; again?: number }) {
  const url = props.url.trim();
  const github = api.onGithub(url);
  const [access, setAccess] = useState<{ url: string; got: Access | null } | null>(null);
  useEffect(() => {
    if (!github) return;
    let gone = false;
    const t = setTimeout(() => {
      api.githubAccess(props.who, url).then(
        (got) => !gone && setAccess({ url, got }),
        () => !gone && setAccess({ url, got: null }),
      );
    }, 500);
    return () => {
      gone = true;
      clearTimeout(t);
    };
  }, [github, url, props.who, props.again]);

  if (!github) return null;
  // checked again (`again`: its secrets may have changed), the last answer stays till the next
  if (access?.url !== url) {
    return (
      <span className="flex items-center gap-1.5 text-muted-foreground">
        <Spinner className="size-3" /> Checking access…
      </span>
    );
  }
  const got = access.got;
  if (!got) return <span className="text-muted-foreground">Access couldn't be checked.</span>;
  if (got.via) {
    const how = { public: "Public", token: "Readable with your GITHUB_TOKEN", app: "Readable through the GitHub App" }[got.via];
    return (
      <span className="flex items-center gap-1.5">
        <CheckIcon className="size-3.5 text-success" /> {how}
      </span>
    );
  }
  return (
    <span className="flex flex-col gap-1">
      <span className="flex items-start gap-1.5 text-destructive">
        <CircleAlertIcon className="mt-px size-3.5 shrink-0" />
        {got.why ?? "dirigent can't read it: it's private, or doesn't exist."}
      </span>
      <span>
        <button type="button" className="text-primary-text hover:underline" onClick={props.addToken}>
          Add a token
        </button>
        {got.app && (
          <>
            {" or "}
            <button type="button" className="text-primary-text hover:underline" onClick={props.connect}>
              connect GitHub
            </button>
          </>
        )}
        .
      </span>
    </span>
  );
}
