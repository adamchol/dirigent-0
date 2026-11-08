// A session's workspace on the user's own machine: `dirigent sync` mirrors it into a folder
// there from the session's log, and keeps it as the session's tools change it. The web only
// shows the commands.

import { useRef } from "react";

import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import type { Who } from "./api.ts";
import { Code } from "./Code.tsx";

/** The commands that mirror session `id` into a folder; open while `id` is. */
export function SyncDialog(props: { id: string | null; who: Who; declared: boolean; close: () => void }) {
  // the id it was opened for, kept while it closes
  const shown = useRef(props.id);
  if (props.id !== null) shown.current = props.id;
  const login = props.declared
    ? `dirigent login ${location.origin} --as ${props.who.user}`
    : `dirigent login ${location.origin}`;
  return (
    <Dialog open={props.id !== null} onOpenChange={(open) => !open && props.close()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Open locally</DialogTitle>
          <DialogDescription>
            Mirror this session's workspace into a folder on your machine, with dirigent's CLI. It keeps up as the
            session's tools change files.
          </DialogDescription>
        </DialogHeader>
        <div className="flex min-w-0 flex-col gap-3">
          <Code code={login} language="bash" title="Sign in to this dirigent, once" />
          <Code code={`dirigent sync ${shown.current ?? ""}`} language="bash" title="Mirror the workspace" />
        </div>
        <ul className="flex list-disc flex-col gap-1 rounded-md bg-panel py-2.5 pr-3 pl-7 text-[13px] text-muted-foreground">
          <li>
            The folder is named after the session. It's a copy to read: a file you change that the session changes too
            is kept as <code className="font-mono text-[12px]">&lt;file&gt;.local</code>.
          </li>
          <li>
            Ignored files, such as dependencies and builds, aren't synced. Install and build there as after a clone.
          </li>
          <li>It needs git.</li>
        </ul>
      </DialogContent>
    </Dialog>
  );
}
