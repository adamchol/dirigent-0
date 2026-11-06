// The sessions the user owns or views, polled from control (it has no push), with their state,
// and starting, stopping and making them.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useLiveQuery } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import {
  BotIcon,
  CodeIcon,
  CopyIcon,
  EllipsisIcon,
  GitBranchIcon,
  OctagonXIcon,
  PlayIcon,
  PlusIcon,
  SquareIcon,
} from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Field, FieldContent, FieldDescription, FieldGroup, FieldLabel, FieldTitle } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupAction,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuAction,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSkeleton,
  SidebarRail,
} from "@/components/ui/sidebar";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import * as api from "./api.ts";
import type { Effort, Part, SessionInfo } from "./api.ts";
import { ACTIVITY_QUERY, millis, type ActivityRow } from "./events.ts";
import logo from "./logo.svg";

const POLL_MS = 2000;

export interface Polled {
  list: SessionInfo[];
  /** Whether a list came. */
  listed: boolean;
  refresh: () => void;
}

/** `who`'s sessions, polled. */
export function useSessions(who: api.Who): Polled {
  const [list, setList] = useState<SessionInfo[]>([]);
  const [listed, setListed] = useState(false);
  const [failing, setFailing] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setList(await api.sessions(who));
      setListed(true);
      setFailing(false);
    } catch (e) {
      // said once, until it works again
      setFailing((was) => {
        if (!was) toast.error("Control is unreachable", { description: (e as Error).message });
        return true;
      });
    }
  }, [who]);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, POLL_MS);
    return () => clearInterval(t);
  }, [refresh]);

  return { list, listed: listed && !failing, refresh };
}

const STATES = {
  active: { label: "Active", dot: "bg-emerald-500", ring: "bg-emerald-500/40" },
  away: { label: "Away", dot: "bg-amber-500", ring: "bg-amber-500/40" },
  idle: { label: "Idle", dot: "bg-muted-foreground/40", ring: "" },
} as const;

/** A session's state as a dot: pulsing while a player runs it. */
export function StateDot({ info, className = "" }: { info: SessionInfo; className?: string }) {
  const s = STATES[api.state(info)];
  return (
    <span className={`relative flex size-2 shrink-0 ${className}`}>
      {s.ring && <span className={`absolute inline-flex size-full animate-ping rounded-full ${s.ring}`} />}
      <span className={`relative inline-flex size-2 rounded-full ${s.dot}`} />
    </span>
  );
}

export function StateBadge({ info }: { info: SessionInfo }) {
  return (
    <Badge variant="secondary" className="gap-1.5 font-normal">
      <StateDot info={info} />
      {STATES[api.state(info)].label}
      <span className="text-muted-foreground tabular-nums">#{info.epoch}</span>
    </Badge>
  );
}

/** Run `f`, then show what it changed; or say why it failed. */
function act(what: string, f: () => Promise<void>, changed: () => void) {
  return async () => {
    try {
      await f();
    } catch (e) {
      toast.error(what, { description: (e as Error).message });
    }
    changed();
  };
}

/** A session's less used things, in a menu: its id, and stopping it (letting the turn finish, or
 * now) for its owner; and starting it, in sight while it's idle. */
export function SessionMenu(props: { id: string; info?: SessionInfo; who: api.Who; changed: () => void }) {
  const { info, who, changed } = props;
  const owner = info?.role === "owner";
  const idle = info !== undefined && api.state(info) === "idle";
  return (
    <>
      {owner && idle && (
        <Button size="sm" onClick={act("Not started", () => api.start(who, props.id), changed)}>
          <PlayIcon /> Start
        </Button>
      )}
      <DropdownMenu>
        <DropdownMenuTrigger render={<Button size="icon-sm" variant="ghost" />}>
          <EllipsisIcon />
          <span className="sr-only">Session</span>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-76">
          <DropdownMenuGroup>
            <DropdownMenuLabel className="flex flex-col gap-0.5">
              Session
              <span className="font-mono text-[0.7rem] font-normal break-all">{props.id}</span>
            </DropdownMenuLabel>
            <DropdownMenuItem onClick={() => navigator.clipboard.writeText(props.id).then(() => toast.success("Session ID copied"))}>
              <CopyIcon /> Copy ID
            </DropdownMenuItem>
          </DropdownMenuGroup>
          {owner && !idle && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuGroup>
                <DropdownMenuItem onClick={act("Not stopped", () => api.stop(who, props.id, false), changed)}>
                  <SquareIcon />
                  <div className="flex flex-col">
                    Stop
                    <span className="text-xs text-muted-foreground">The turn finishes first</span>
                  </div>
                </DropdownMenuItem>
                <DropdownMenuItem variant="destructive" onClick={act("Not stopped", () => api.stop(who, props.id, true), changed)}>
                  <OctagonXIcon />
                  <div className="flex flex-col">
                    Stop now
                    <span className="text-xs">The turn is cancelled</span>
                  </div>
                </DropdownMenuItem>
              </DropdownMenuGroup>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </>
  );
}

const MotionItem = motion.create(SidebarMenuItem);

export function SessionsSidebar(props: {
  sessions: Polled;
  selected: string;
  select: (id: string) => void;
  who: api.Who;
  create: () => void;
  account: React.ReactNode;
}) {
  const { listed } = props.sessions;
  // the most recently active first: its latest event, or its making (one just made has none yet)
  const activity = useLiveQuery<ActivityRow & Record<string, unknown>>(ACTIVITY_QUERY);
  const active = useMemo(() => {
    const last = new Map(activity.rows.map((r) => [r.session_id, millis(r.last)]));
    return new Map(props.sessions.list.map((s) => [s.id, Math.max(s.created, last.get(s.id) || 0)]));
  }, [activity.rows, props.sessions.list]);
  const list = [...props.sessions.list].sort((a, b) => active.get(b.id)! - active.get(a.id)!);
  const mine = list.filter((s) => s.role === "owner");
  const shared = list.filter((s) => s.role !== "owner");
  return (
    <Sidebar>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" render={<a href="#" />}>
              <img src={logo} alt="" className="size-8 rounded-lg ring-1 ring-border" />
              <div className="grid flex-1 text-left leading-tight">
                <span className="font-semibold">dirigent</span>
                <span className="text-xs text-muted-foreground">coding agents, conducted</span>
              </div>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
        <Button variant="outline" className="justify-start" onClick={props.create}>
          <PlusIcon /> New session
        </Button>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupLabel>Sessions</SidebarGroupLabel>
          <SidebarGroupAction title="New session" onClick={props.create}>
            <PlusIcon />
            <span className="sr-only">New session</span>
          </SidebarGroupAction>
          <SidebarGroupContent>
            <SidebarMenu>
              {!listed && list.length === 0 && [0, 1, 2].map((i) => <SidebarMenuSkeleton key={i} showIcon />)}
              {listed && mine.length === 0 && (
                <p className="px-2 py-1.5 text-xs text-muted-foreground">None yet: make one.</p>
              )}
              <SessionItems list={mine} {...props} />
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
        {shared.length > 0 && (
          <SidebarGroup>
            <SidebarGroupLabel>Shared with you</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                <SessionItems list={shared} {...props} />
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        )}
      </SidebarContent>
      <SidebarFooter>{props.account}</SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}

function SessionItems(props: {
  list: SessionInfo[];
  selected: string;
  select: (id: string) => void;
  who: api.Who;
  sessions: Polled;
}) {
  return (
    <AnimatePresence initial={false}>
      {props.list.map((s) => {
        const owner = s.role === "owner";
        const st = api.state(s);
        return (
          <MotionItem
            key={s.id}
            layout="position"
            initial={{ opacity: 0, x: -8 }}
            animate={{ opacity: 1, x: 0 }}
            exit={{ opacity: 0, x: -8 }}
            transition={{ duration: 0.2 }}
          >
            <SidebarMenuButton isActive={s.id === props.selected} onClick={() => props.select(s.id)} title={api.title(s)}>
              <span className="truncate">{api.title(s)}</span>
            </SidebarMenuButton>
            <DropdownMenu>
              <DropdownMenuTrigger render={<SidebarMenuAction showOnHover />}>
                <EllipsisIcon />
                <span className="sr-only">More</span>
              </DropdownMenuTrigger>
              <DropdownMenuContent side="right" align="start" className="w-48">
                <DropdownMenuGroup>
                  {owner && st === "idle" && (
                    <DropdownMenuItem onClick={act("Not started", () => api.start(props.who, s.id), props.sessions.refresh)}>
                      <PlayIcon /> Start
                    </DropdownMenuItem>
                  )}
                  {owner && st !== "idle" && (
                    <>
                      <DropdownMenuItem onClick={act("Not stopped", () => api.stop(props.who, s.id, false), props.sessions.refresh)}>
                        <SquareIcon /> Stop
                      </DropdownMenuItem>
                      <DropdownMenuItem
                        variant="destructive"
                        onClick={act("Not stopped", () => api.stop(props.who, s.id, true), props.sessions.refresh)}
                      >
                        <OctagonXIcon /> Stop now
                      </DropdownMenuItem>
                    </>
                  )}
                </DropdownMenuGroup>
                {owner && <DropdownMenuSeparator />}
                <DropdownMenuItem onClick={() => navigator.clipboard.writeText(s.id).then(() => toast.success("Session id copied"))}>
                  <CopyIcon /> Copy id
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </MotionItem>
        );
      })}
    </AnimatePresence>
  );
}

const EFFORTS: { value: Effort | "none"; label: string }[] = [
  { value: "none", label: "No thinking" },
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "max", label: "Max" },
];

/** Making a session, `who`'s: a coder on a repository, or a dirigent. */
export function NewSession(props: { open: boolean; setOpen: (open: boolean) => void; who: api.Who; made: (id: string) => void }) {
  const [part, setPart] = useState<Part>("coder");
  const [git, setGit] = useState("");
  const [ref, setRef] = useState("");
  const [model, setModel] = useState("claude-sonnet-5");
  const [effort, setEffort] = useState<Effort | "none">("medium");
  const [start, setStart] = useState(true);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const id = crypto.randomUUID();
    const source =
      part === "coder" && git.trim() ? { git: { url: git.trim(), ...(ref.trim() ? { ref: ref.trim() } : {}) } } : null;
    setBusy(true);
    try {
      await api.put(props.who, id, { model, effort: effort === "none" ? null : effort, source, part }, start);
      props.setOpen(false);
      props.made(id);
    } catch (err) {
      toast.error("Not made", { description: (err as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={props.open} onOpenChange={props.setOpen}>
      <DialogContent className="sm:max-w-lg">
        <form onSubmit={submit} className="flex flex-col gap-6">
          <DialogHeader>
            <DialogTitle>New session</DialogTitle>
            <DialogDescription>Yours, {props.who.user}: a player runs it in its own VM.</DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <ToggleGroup
              value={[part]}
              onValueChange={(v: string[]) => v[0] && setPart(v[0] as Part)}
              variant="outline"
              className="grid w-full grid-cols-2"
            >
              <ToggleGroupItem value="coder" className="h-auto flex-col items-start gap-0.5 p-3 text-left">
                <span className="flex items-center gap-1.5 font-medium">
                  <CodeIcon className="size-4" /> Coder
                </span>
                <span className="text-xs font-normal text-muted-foreground">Works in a repository</span>
              </ToggleGroupItem>
              <ToggleGroupItem value="dirigent" className="h-auto flex-col items-start gap-0.5 p-3 text-left">
                <span className="flex items-center gap-1.5 font-medium">
                  <BotIcon className="size-4" /> Dirigent
                </span>
                <span className="text-xs font-normal text-muted-foreground">Directs your sessions</span>
              </ToggleGroupItem>
            </ToggleGroup>
            <AnimatePresence initial={false}>
              {part === "coder" && (
                <motion.div
                  initial={{ height: 0, opacity: 0 }}
                  animate={{ height: "auto", opacity: 1 }}
                  exit={{ height: 0, opacity: 0 }}
                  transition={{ duration: 0.25, ease: [0.23, 1, 0.32, 1] }}
                  // the group's gap above it, inside it instead (-mt-5 pt-5), so it goes with it: left
                  // outside, it would vanish at once as the block's removed
                  className="-mt-5 overflow-hidden"
                >
                  <div className="grid gap-4 pt-5 sm:grid-cols-[1fr_10rem]">
                    <Field>
                      <FieldLabel htmlFor="git">Repository</FieldLabel>
                      <InputGroup>
                        <InputGroupAddon>
                          <GitBranchIcon />
                        </InputGroupAddon>
                        <InputGroupInput id="git" placeholder="https://github.com/…" value={git} onChange={(e) => setGit(e.target.value)} />
                      </InputGroup>
                      <FieldDescription>Empty: a workspace with no files.</FieldDescription>
                    </Field>
                    <Field>
                      <FieldLabel htmlFor="ref">Ref</FieldLabel>
                      <Input id="ref" placeholder="default branch" value={ref} onChange={(e) => setRef(e.target.value)} />
                    </Field>
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
            <div className="grid gap-4 sm:grid-cols-[1fr_10rem]">
              <Field>
                <FieldLabel htmlFor="model">Model</FieldLabel>
                <Input id="model" value={model} onChange={(e) => setModel(e.target.value)} required />
              </Field>
              <Field>
                <FieldLabel>Thinking</FieldLabel>
                <Select items={EFFORTS} value={effort} onValueChange={(v) => v && setEffort(v as Effort | "none")}>
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
            <Field orientation="horizontal">
              <FieldContent>
                <FieldTitle>Start it</FieldTitle>
                <FieldDescription>A player takes it at once.</FieldDescription>
              </FieldContent>
              <Switch checked={start} onCheckedChange={setStart} />
            </Field>
          </FieldGroup>
          <DialogFooter>
            <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
            <Button type="submit" disabled={busy}>
              {busy ? <Spinner /> : <PlusIcon />} Make it
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
