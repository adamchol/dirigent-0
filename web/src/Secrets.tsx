// The secrets sessions' tools get: given to control (never written into a log), for one session
// or all the user's, and sent to their players on their leases. Here the owner gives the one a
// dirigent asked for, and keeps the rest: names only, as values are never shown again.

import { useCallback, useEffect, useState } from "react";
import { KeyRoundIcon, PlusIcon, Trash2Icon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import * as api from "./api.ts";
import type { SessionInfo, Who } from "./api.ts";
import type { Item } from "./events.ts";

export type Request = Extract<Item, { t: "request" }>;

/** Give the secret `request` asks for, for its session or all the user's; then `provided`. */
export function ProvideDialog(props: {
  request: Request | null;
  /** The session's name, as the user knows it. */
  title: string;
  who: Who;
  provided: (request: Request) => Promise<void>;
  close: () => void;
}) {
  const [value, setValue] = useState("");
  const [scope, setScope] = useState<"session" | "all">("session");
  const [busy, setBusy] = useState(false);
  const open = props.request !== null;
  useEffect(() => {
    if (open) {
      setValue("");
      setScope("session");
    }
  }, [open]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const r = props.request;
    if (!r) return;
    setBusy(true);
    try {
      await api.setSecret(props.who, r.name, value, scope === "session" ? r.session : undefined);
      await props.provided(r);
      props.close();
    } catch (err) {
      toast.error("Not provided", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={submit} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>Provide {props.request?.name}</DialogTitle>
            <DialogDescription>
              For {props.title}: {props.request?.why}
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="secret-value">Value</FieldLabel>
              <Input
                id="secret-value"
                type="password"
                autoComplete="off"
                value={value}
                onChange={(e) => setValue(e.target.value)}
                required
                autoFocus
              />
              <FieldDescription>It goes to the session's tools, never into the chat. It isn't shown again.</FieldDescription>
            </Field>
            <Field>
              <FieldLabel>For</FieldLabel>
              <ToggleGroup
                variant="outline"
                className="grid w-full grid-cols-2"
                value={[scope]}
                onValueChange={(v: string[]) => v[0] && setScope(v[0] as "session" | "all")}
              >
                <ToggleGroupItem value="session">This session</ToggleGroupItem>
                <ToggleGroupItem value="all">All my sessions</ToggleGroupItem>
              </ToggleGroup>
            </Field>
          </FieldGroup>
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy || !value}>
              {busy ? <Spinner /> : <KeyRoundIcon />} Provide
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The secrets of `session` (its own; its owner's for all sessions shown too), or, without one,
 * the user's for all their sessions: added, replaced and removed by name. */
export function SecretsDialog(props: { open: boolean; who: Who; session?: SessionInfo; close: () => void }) {
  const id = props.session?.id;
  const [names, setNames] = useState<string[] | null>(null);
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const load = useCallback(() => {
    api.secrets(props.who, id).then(setNames, (e: Error) => toast.error("Secrets not read", { description: e.message }));
  }, [props.who, id]);
  useEffect(() => {
    if (!props.open) return;
    setNames(null);
    setName("");
    setValue("");
    load();
  }, [props.open, load]);
  // a session's, from all its owner's sessions: what control lists that isn't its own
  const inherited = (props.session?.secrets ?? []).filter((n) => !names?.includes(n));
  const valid = api.secretName(name);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      await api.setSecret(props.who, name, value, id);
      setName("");
      setValue("");
      load();
    } catch (err) {
      toast.error("Not saved", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  async function remove(n: string) {
    try {
      await api.removeSecret(props.who, n, id);
      load();
    } catch (err) {
      toast.error("Not removed", { description: (err as Error).message });
    }
  }

  return (
    <Dialog open={props.open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{props.session ? "Session secrets" : "Secrets"}</DialogTitle>
          <DialogDescription>
            {props.session
              ? "Environment variables this session's tools get. Its own win over those for all your sessions."
              : "Environment variables every session of yours gets. A session's own of the same name wins."}
          </DialogDescription>
        </DialogHeader>
        <ul className="flex flex-col divide-y divide-border rounded-md border text-[13px]">
          {names === null ? (
            <li className="flex justify-center p-3">
              <Spinner />
            </li>
          ) : names.length === 0 && inherited.length === 0 ? (
            <li className="p-3 text-muted-foreground">None yet.</li>
          ) : (
            <>
              {names.map((n) => (
                <li key={n} className="flex items-center gap-2 py-1 pr-1 pl-3">
                  <KeyRoundIcon className="size-3.5 text-muted-foreground" />
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{n}</span>
                  <Button size="icon-sm" variant="ghost" onClick={() => remove(n)} aria-label={`Remove ${n}`}>
                    <Trash2Icon />
                  </Button>
                </li>
              ))}
              {inherited.map((n) => (
                <li key={n} className="flex items-center gap-2 py-2 pr-3 pl-3 text-muted-foreground">
                  <KeyRoundIcon className="size-3.5" />
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{n}</span>
                  <span className="text-[11px]">all sessions</span>
                </li>
              ))}
            </>
          )}
        </ul>
        <form onSubmit={add} className="flex flex-col gap-3">
          <div className="flex gap-2">
            <Input
              aria-label="Name"
              placeholder="NAME"
              className="font-mono"
              value={name}
              onChange={(e) => setName(e.target.value.toUpperCase())}
              aria-invalid={name !== "" && !valid}
            />
            <Input
              aria-label="Value"
              type="password"
              autoComplete="off"
              placeholder="Value"
              value={value}
              onChange={(e) => setValue(e.target.value)}
            />
          </div>
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Done</DialogClose>
            <Button type="submit" disabled={busy || !valid || !value}>
              {busy ? <Spinner /> : <PlusIcon />} {names?.includes(name) ? "Replace" : "Add"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
