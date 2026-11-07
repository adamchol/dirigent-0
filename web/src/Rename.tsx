// Renaming a session: its owner writes a `titled` event to its log, as they write messages, and
// may hold the name, so the player doesn't name the session again after a compaction.

import { useEffect, useMemo, useState } from "react";
import { useLiveQuery } from "@chlive/client/react";
import type { LiveClient } from "@chlive/client";
import { PencilIcon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Field, FieldContent, FieldDescription, FieldGroup, FieldLabel, FieldTitle } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import * as api from "./api.ts";
import type { SessionInfo } from "./api.ts";
import { TITLES_QUERY, titles, type Named, type TitleRow } from "./events.ts";

/** Every session's latest title, live. */
export function useTitles(): Map<string, Named> {
  const rows = useLiveQuery<TitleRow & Record<string, unknown>>(TITLES_QUERY);
  return useMemo(() => titles(rows.rows), [rows.rows]);
}

/** The dialog renaming the session `info`, once there is one to rename. */
export function RenameDialog(props: { info?: SessionInfo; client: LiveClient; close: () => void }) {
  const open = props.info !== undefined;
  const named = useTitles().get(props.info?.id ?? "");
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [hold, setHold] = useState(true);
  const [busy, setBusy] = useState(false);
  // as it's named when it opens
  useEffect(() => {
    if (!open) return;
    setTitle(named?.title ?? "");
    setDescription(named?.description ?? "");
    setHold(true);
    // not as it's named again meanwhile: what's typed stays
  }, [open]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!props.info) return;
    setBusy(true);
    try {
      const named: Named = { title: title.trim(), description: description.trim(), ...(hold ? { hold } : {}) };
      await props.client.insert("session_events", {
        session_id: props.info.id,
        id: crypto.randomUUID(),
        kind: "titled",
        data: JSON.stringify(named),
      });
      props.close();
    } catch (err) {
      toast.error("Not renamed", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>Rename session</DialogTitle>
            <DialogDescription>{props.info ? api.place(props.info) : ""}</DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="title">Title</FieldLabel>
              <Input id="title" value={title} onChange={(e) => setTitle(e.target.value)} maxLength={80} required autoFocus />
            </Field>
            <Field>
              <FieldLabel htmlFor="description">Description</FieldLabel>
              <Textarea
                id="description"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                maxLength={300}
                placeholder="What the session is doing"
              />
            </Field>
            <Field orientation="horizontal">
              <FieldContent>
                <FieldTitle>Keep this name</FieldTitle>
                <FieldDescription>Off, the player names it again as the work moves on.</FieldDescription>
              </FieldContent>
              <Switch checked={hold} onCheckedChange={setHold} />
            </Field>
          </FieldGroup>
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy || !title.trim()}>
              {busy ? <Spinner /> : <PencilIcon />} Rename
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
