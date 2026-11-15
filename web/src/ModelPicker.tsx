/** Who pays, which model, how hard it thinks: a session's settings, as it's made or switched. */
import { useEffect, useState } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import * as api from "./api.ts";
import type { Effort, Me, Model, ModelSettings, Provider, SessionInfo, Who } from "./api.ts";
import { linked } from "./Link.tsx";

const EFFORTS: { value: Effort | "none"; label: string }[] = [
  { value: "none", label: "No thinking" },
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "max", label: "Max" },
];

const PROVIDERS: { value: Provider; label: string }[] = [
  { value: "anthropic", label: "Anthropic API" },
  { value: "chatgpt", label: "ChatGPT plan" },
];

/** A new session's model, on Anthropic. */
export const CLAUDE = "claude-sonnet-5";

export const DEFAULT_SETTINGS: ModelSettings = { model: CLAUDE, effort: "medium", provider: "anthropic" };

export function ModelPicker(props: {
  who: Who;
  me: Me | null;
  /** While it's shown: the plan's models are listed then. */
  open: boolean;
  value: ModelSettings;
  onChange: (s: ModelSettings) => void;
  /** Link a ChatGPT plan. */
  connect: () => void;
}) {
  const [models, setModels] = useState<Model[] | null>(null);
  const { value, onChange, who, open } = props;
  const plan = linked(props.me);

  // the plan's models, once it's chosen: the first, unless one of them is
  useEffect(() => {
    if (value.provider !== "chatgpt" || !open) return;
    let gone = false;
    api.models(who).then(
      (list) => {
        if (gone) return;
        setModels(list);
        if (!list.some((x) => x.slug === value.model)) onChange({ ...value, model: list[0]?.slug ?? "" });
      },
      (err) => !gone && toast.error("The plan's models aren't listed", { description: (err as Error).message }),
    );
    return () => {
      gone = true;
    };
    // listed again only when the provider or who asks changes
  }, [value.provider, open, who]);

  function choose(p: Provider) {
    if (p === value.provider) return;
    onChange({ ...value, provider: p, model: p === "anthropic" ? CLAUDE : "" });
  }

  return (
    <>
      <Field>
        <FieldLabel>Paid with</FieldLabel>
        <Select items={PROVIDERS} value={value.provider} onValueChange={(v) => v && choose(v as Provider)}>
          <SelectTrigger className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {PROVIDERS.map((p) => (
              <SelectItem key={p.value} value={p.value} disabled={p.value === "chatgpt" && !plan}>
                {p.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {!plan && (
          <FieldDescription>
            {props.me?.chatgpt?.broken ? "Your ChatGPT link is broken: " : "No ChatGPT plan is linked: "}
            <button type="button" className="text-primary-text hover:underline" onClick={props.connect}>
              link it
            </button>
            .
          </FieldDescription>
        )}
      </Field>
      <div className="grid gap-4 sm:grid-cols-[1fr_10rem]">
        <Field>
          <FieldLabel htmlFor="model">Model</FieldLabel>
          {value.provider === "chatgpt" ? (
            <Select
              items={(models ?? []).map((m) => ({ value: m.slug, label: m.display_name }))}
              value={value.model}
              onValueChange={(v) => v && onChange({ ...value, model: v as string })}
            >
              <SelectTrigger className="w-full" id="model" disabled={!models}>
                <SelectValue placeholder={models ? "No models" : "Listing…"} />
              </SelectTrigger>
              <SelectContent>
                {(models ?? []).map((m) => (
                  <SelectItem key={m.slug} value={m.slug}>
                    {m.display_name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <Input id="model" value={value.model} onChange={(e) => onChange({ ...value, model: e.target.value })} required />
          )}
        </Field>
        <Field>
          <FieldLabel>Thinking</FieldLabel>
          <Select
            items={EFFORTS}
            value={value.effort ?? "none"}
            onValueChange={(v) => v && onChange({ ...value, effort: v === "none" ? null : (v as Effort) })}
          >
            <SelectTrigger className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {EFFORTS.map((e) => (
                <SelectItem key={e.value} value={e.value}>
                  {e.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
      </div>
    </>
  );
}

/** Switch a session, its owner's, to another model, effort or provider: running, it takes them
 * from its next model call; idle, its next player starts with them. */
export function ModelDialog(props: {
  info?: SessionInfo;
  who: Who;
  me: Me | null;
  connect: () => void;
  changed: () => void;
  close: () => void;
}) {
  const { info } = props;
  const [value, setValue] = useState<ModelSettings>(DEFAULT_SETTINGS);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (info) setValue(api.settingsOf(info));
    // as the session is when it's opened, not as it's polled meanwhile
  }, [info?.id]);
  const same = info !== undefined && JSON.stringify(api.settingsOf(info)) === JSON.stringify(value);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!info) return;
    setBusy(true);
    try {
      await api.setModel(props.who, info.id, value);
      toast.success(`Now ${api.settingsLine(value)}`);
      props.changed();
      props.close();
    } catch (err) {
      toast.error("Not switched", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={info !== undefined} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-lg">
        <form onSubmit={submit} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>Model</DialogTitle>
            <DialogDescription>
              {info && api.state(info) !== "idle"
                ? "It takes the new model from its next call; one running finishes as it began."
                : "Its next start runs it."}{" "}
              The next call reads the whole conversation afresh (uncached) once, and thinking doesn't carry over between
              providers.
            </DialogDescription>
          </DialogHeader>
          <ModelPicker
            who={props.who}
            me={props.me}
            open={info !== undefined}
            value={value}
            onChange={setValue}
            connect={props.connect}
          />
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy || same || !value.model.trim()}>
              {busy && <Spinner />} Switch
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
