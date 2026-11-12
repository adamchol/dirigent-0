// The user's settings, which control keeps (so they follow them to any browser): for now, how
// much of a session's log the chat shows. And, told not set, the system sessions' machines run.

import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import * as api from "./api.ts";
import type { PlayerSystem, Settings, Who } from "./api.ts";
import type { Shown } from "./events.ts";

/** What's shown until control answers, and if it can't. */
export const DEFAULT_SETTINGS: Settings = { events: "activity" };

/** `who`'s settings, and changing them: shown at once, then kept by control (put back as they
 * were, if it refuses). */
export function useSettings(who: Who): { settings: Settings; change: (s: Partial<Settings>) => void } {
  const [settings, setSettings] = useState<Settings>(DEFAULT_SETTINGS);
  useEffect(() => {
    api.settings(who).then(setSettings, () => {});
  }, [who]);
  const change = useCallback(
    (part: Partial<Settings>) => {
      setSettings((was) => {
        const next = { ...was, ...part };
        api.saveSettings(who, next).then(setSettings, (err: Error) => {
          setSettings(was);
          toast.error("Settings not saved", { description: err.message });
        });
        return next;
      });
    },
    [who],
  );
  return { settings, change };
}

/** Which system new sessions' machines run, as the cluster has it. */
function machines(s: PlayerSystem): string {
  if (!s.configured) return "The stock system.";
  switch (s.state) {
    case "ready":
      return "The cluster's own system.";
    case "building":
      return "The stock system, while the cluster's own is built.";
    case "broken":
      return "The stock system: sessions' machines didn't come up with the cluster's own.";
    default:
      return "The stock system: the cluster's own failed to build.";
  }
}

const SHOWN: { value: Shown; label: string; description: string }[] = [
  { value: "conversation", label: "Conversation", description: "Messages, replies, tools, turns and errors." },
  { value: "activity", label: "Activity", description: "The player's doings too: attaching, the workspace, checkpoints, titles." },
  { value: "all", label: "All events", description: "Every event in the log, each with its JSON (but the streamed deltas and workspace files)." },
];

export function SettingsDialog(props: {
  open: boolean;
  who: Who;
  settings: Settings;
  change: (s: Partial<Settings>) => void;
  close: () => void;
}) {
  const shown = SHOWN.find((s) => s.value === props.settings.events) ?? SHOWN[1]!;
  const [system, setSystem] = useState<PlayerSystem | null>(null);
  useEffect(() => {
    if (props.open) api.playerSystem(props.who).then(setSystem, () => setSystem(null));
  }, [props.open, props.who]);
  return (
    <Dialog open={props.open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Settings</DialogTitle>
          <DialogDescription>Yours, in every browser you sign in with.</DialogDescription>
        </DialogHeader>
        <Field>
          <FieldLabel>The chat shows</FieldLabel>
          <ToggleGroup
            variant="outline"
            className="grid w-full grid-cols-3"
            value={[props.settings.events]}
            onValueChange={(v: string[]) => v[0] && props.change({ events: v[0] as Shown })}
          >
            {SHOWN.map((s) => (
              <ToggleGroupItem key={s.value} value={s.value}>
                {s.label}
              </ToggleGroupItem>
            ))}
          </ToggleGroup>
          <FieldDescription>{shown.description}</FieldDescription>
        </Field>
        {system && (
          <Field>
            <FieldLabel>New sessions' machines run</FieldLabel>
            <FieldDescription>{machines(system)}</FieldDescription>
          </Field>
        )}
        <DialogFooter>
          <DialogClose render={<Button type="button" variant="outline" />}>Done</DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
