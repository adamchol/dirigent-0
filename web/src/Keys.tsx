// The Anthropic API key a user's sessions run on: given to control, which checks it with
// Anthropic, keeps it encrypted and hands it to their players on their leases; only its last four
// characters are shown again. Without one, the install's key, if it has one.

import { useEffect, useState } from "react";
import { KeyRoundIcon, Trash2Icon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import * as api from "./api.ts";
import type { Me, Who } from "./api.ts";

export function AnthropicKeyDialog(props: { open: boolean; who: Who; me: Me | null; changed: () => void; close: () => void }) {
  const [key, setKey] = useState("");
  const [refused, setRefused] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (props.open) {
      setKey("");
      setRefused(null);
    }
  }, [props.open]);
  const own = props.me?.anthropic;

  async function save(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setRefused(null);
    try {
      await api.setAnthropicKey(props.who, key.trim());
      props.changed();
      props.close();
      toast.success("Anthropic key saved");
    } catch (err) {
      // control's word: Anthropic's refusal, as it said it
      setRefused((err as Error).message.replace(/^\d+: /, ""));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    try {
      await api.removeAnthropicKey(props.who);
      props.changed();
      props.close();
    } catch (err) {
      toast.error("Not removed", { description: (err as Error).message });
    }
  }

  return (
    <Dialog open={props.open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={save} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>Anthropic API key</DialogTitle>
            <DialogDescription>
              Your sessions on Claude run on it.{" "}
              {own
                ? `Yours ends in …${own.hint}.`
                : props.me?.anthropic_default
                  ? "Until you give one, they run on this dirigent's own key."
                  : "This dirigent has no key of its own, so they need yours."}
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <Field data-invalid={refused !== null || undefined}>
              <FieldLabel htmlFor="anthropic-key">{own ? "Replace it with" : "Key"}</FieldLabel>
              <Input
                id="anthropic-key"
                type="password"
                autoComplete="off"
                placeholder="sk-ant-…"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                aria-invalid={refused !== null || undefined}
                autoFocus
              />
              {refused ? (
                <FieldError>{refused}</FieldError>
              ) : (
                <FieldDescription>Checked with Anthropic, then kept encrypted. Only its last four characters are shown again.</FieldDescription>
              )}
            </Field>
          </FieldGroup>
          <DialogFooter>
            {own && (
              <Button type="button" variant="ghost" className="mr-auto text-destructive" onClick={remove}>
                <Trash2Icon /> Remove
              </Button>
            )}
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy || !key.trim()}>
              {busy ? <Spinner /> : <KeyRoundIcon />} Save
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
