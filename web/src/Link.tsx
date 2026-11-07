// The ChatGPT plan a user's sessions may run on: linked through the CLI (OpenAI's sign-in comes
// back only to a loopback address, which only a process on the user's machine has), so the web
// shows the commands and watches for the link to appear.

import { useCallback, useEffect, useState } from "react";
import { CircleAlertIcon, CircleCheckIcon } from "lucide-react";

import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import * as api from "./api.ts";
import type { Me, Who } from "./api.ts";
import { Code } from "./Code.tsx";

/** `who` to control, with their link: read again every `every` ms. */
export function useMe(who: Who, every: number): { me: Me | null; refresh: () => void } {
  const [me, setMe] = useState<Me | null>(null);
  const refresh = useCallback(() => {
    api.me(who).then(setMe, () => {});
  }, [who]);
  useEffect(() => {
    refresh();
    const t = setInterval(refresh, every);
    return () => clearInterval(t);
  }, [refresh, every]);
  return { me, refresh };
}

/** Whether `me`'s plan can pay for a session now. */
export function linked(me: Me | null): boolean {
  return !!me?.chatgpt && !me.chatgpt.broken;
}

/** The commands that link a ChatGPT plan, and whether it's linked, as it changes. */
export function ChatGptDialog(props: { who: Who; declared: boolean; open: boolean; setOpen: (open: boolean) => void }) {
  // while it's open, quickly: the link appears as the CLI finishes
  const { me } = useMe(props.who, props.open ? 2000 : 60000);
  const login = props.declared
    ? `dirigent login ${location.origin} --as ${props.who.user}`
    : `dirigent login ${location.origin}`;
  const link = me?.chatgpt;
  return (
    <Dialog open={props.open} onOpenChange={props.setOpen}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>ChatGPT plan</DialogTitle>
          <DialogDescription>
            Your sessions' model calls can be paid with your ChatGPT Plus or Pro plan. OpenAI's sign-in comes back to
            your own machine, so it's done with dirigent's CLI there.
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <Code code={login} language="bash" title="Sign in to this dirigent" />
          <Code code="dirigent link chatgpt" language="bash" title="Link your plan, in your browser" />
        </div>
        <div className="flex items-start gap-2 rounded-md bg-panel px-3 py-2.5 text-[13px]">
          {!link ? (
            <span className="text-muted-foreground">Not linked yet. This updates once it is.</span>
          ) : link.broken ? (
            <>
              <CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
              <span>
                <span className="text-strong">{link.email}</span>'s link is broken: {link.broken}. Run{" "}
                <code className="font-mono text-[12px]">dirigent link chatgpt</code> again.
              </span>
            </>
          ) : (
            <>
              <CircleCheckIcon className="mt-0.5 size-4 shrink-0 text-success" />
              <span>
                Linked: <span className="text-strong">{link.email}</span>. New sessions can run on it.
              </span>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
