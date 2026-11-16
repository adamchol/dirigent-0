/** A session's machine: its fields, as it's made or changed, and the dialog changing it. */
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
import { Field, FieldLabel } from "@/components/ui/field";
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import * as api from "./api.ts";
import type { Machines, Resources, SessionInfo, Who } from "./api.ts";
import * as machine from "./machine.ts";

const PARTS: { part: keyof Resources; label: string; unit: string }[] = [
  { part: "cpu", label: "Cores", unit: "CPU" },
  { part: "memory", label: "Memory", unit: "GiB" },
  { part: "disk", label: "Disk", unit: "GiB" },
];

/** A machine's parts, as typed (`shown`), within `limits` and at least `min`. */
export function MachineFields(props: {
  shown: (part: keyof Resources) => string;
  type: (part: keyof Resources, value: string) => void;
  limits: Partial<Resources>;
  min?: Partial<Resources>;
}) {
  return (
    <div className="grid grid-cols-3 gap-4">
      {PARTS.map((m) => (
        <Field key={m.part}>
          <FieldLabel htmlFor={`machine-${m.part}`}>{m.label}</FieldLabel>
          <InputGroup>
            <InputGroupInput
              id={`machine-${m.part}`}
              type="number"
              inputMode="numeric"
              min={props.min?.[m.part] ?? 1}
              max={props.limits[m.part]}
              value={props.shown(m.part)}
              onChange={(e) => props.type(m.part, e.target.value)}
            />
            <InputGroupAddon align="inline-end">{m.unit}</InputGroupAddon>
          </InputGroup>
        </Field>
      ))}
    </div>
  );
}

/** Changing `info`'s machine: an idle session starts on it next; an active one moves to it once
 * its turn is done. Its disk only grows. */
export function MachineDialog(props: { info?: SessionInfo; who: Who; changed: () => void; close: () => void }) {
  const { info } = props;
  const [machines, setMachines] = useState<Machines | null>(null);
  const [typed, setTyped] = useState<Partial<Record<keyof Resources, string>>>({});
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!info) return;
    setTyped({});
    api.machines(props.who).then(setMachines, () => setMachines(null));
    // as the session is when it's opened, not as it's polled meanwhile
  }, [info?.id, props.who]);
  // control's: its own, or the cluster's default as it is now
  const current = info?.spec.resources ?? machines?.defaults;
  const shown = (part: keyof Resources) => typed[part] ?? String(current?.[part] ?? "");
  const chosen: Resources = { cpu: Number(shown("cpu")), memory: Number(shown("memory")), disk: Number(shown("disk")) };
  const same = current !== undefined && machine.same(chosen, current);
  const active = info !== undefined && api.state(info) !== "idle";

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!info || !machines || !current) return;
    const wrong =
      machine.wrong(chosen, machines.limits) ??
      (chosen.disk < current.disk ? `Disk: at least ${current.disk}, its workspace's now.` : null);
    if (wrong) {
      toast.error("Not that machine", { description: wrong });
      return;
    }
    setBusy(true);
    try {
      const moves = await api.setMachine(props.who, info.id, chosen);
      toast.success(moves ? "Moves to the new machine after its turn" : "Starts on the new machine next", {
        description: machine.summary(chosen),
      });
      props.changed();
      props.close();
    } catch (err) {
      toast.error("Not changed", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={info !== undefined} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-lg">
        <form onSubmit={submit} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>Machine</DialogTitle>
            <DialogDescription>
              {active
                ? "It moves: its player stops once the turn running is done, and the next starts on the new machine, its workspace as it was checkpointed."
                : "Its next start runs on it."}{" "}
              The disk only grows.
            </DialogDescription>
          </DialogHeader>
          {machines && current ? (
            <MachineFields
              shown={shown}
              type={(part, value) => setTyped((t) => ({ ...t, [part]: value }))}
              limits={machines.limits}
              min={{ disk: current.disk }}
            />
          ) : (
            <Spinner />
          )}
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy || same || !machines}>
              {busy && <Spinner />} {active ? "Move" : "Change"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
