// The sessions the user owns or views, polled from control (it has no push), with their state,
// and starting, stopping and making them.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useLiveQuery } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import {
  ArchiveIcon,
  ArchiveRestoreIcon,
  BotIcon,
  ChevronRightIcon,
  CodeIcon,
  CopyIcon,
  EllipsisIcon,
  FolderDownIcon,
  GitBranchIcon,
  LockKeyholeIcon,
  OctagonXIcon,
  PencilIcon,
  PlayIcon,
  PlusIcon,
  SquareIcon,
  Trash2Icon,
} from "lucide-react";
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
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { cn } from "@/lib/utils";
import * as api from "./api.ts";
import type { Effort, Me, Model, Part, Provider, SessionInfo } from "./api.ts";
import { RepoAccess } from "./GitHub.tsx";
import { linked } from "./Link.tsx";
import { useTitles } from "./Rename.tsx";
import type { Named } from "./events.ts";
import { ACTIVITY_QUERY, millis, type ActivityRow } from "./events.ts";

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

const DOTS = {
  active: "bg-primary shadow-[0_0_0_3px_color-mix(in_oklab,var(--primary)_22%,transparent)]",
  away: "bg-dim",
  idle: "bg-transparent ring-1 ring-dim ring-inset",
} as const;

const STATE_NAMES = { active: "running", away: "its player away", idle: "idle" } as const;

/** A session's state as a small dot: the accent, haloed, while a player runs it; grey while its
 * player's away; a ring while idle. */
export function StateDot({ info, className = "" }: { info: SessionInfo; className?: string }) {
  return <span aria-hidden="true" className={cn("inline-block size-1.5 shrink-0 rounded-full", DOTS[api.state(info)], className)} />;
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

/** Archiving the session, or unarchiving it, and deleting it once it's archived (`remove`): its
 * owner's, while it's idle. */
function ArchiveItems(props: { info: SessionInfo; who: api.Who; changed: () => void; remove: () => void }) {
  const on = !props.info.archived;
  return (
    <>
      <DropdownMenuItem
        onClick={act(on ? "Not archived" : "Not unarchived", () => api.archive(props.who, props.info.id, on), props.changed)}
      >
        {on ? <ArchiveIcon /> : <ArchiveRestoreIcon />} {on ? "Archive" : "Unarchive"}
      </DropdownMenuItem>
      {!on && (
        <DropdownMenuItem variant="destructive" onClick={props.remove}>
          <Trash2Icon /> Delete…
        </DropdownMenuItem>
      )}
    </>
  );
}

/** The dialog deleting the session `info` (archived), once there is one to delete; `deleted`,
 * once control took it. */
export function DeleteDialog(props: { info?: SessionInfo; who: api.Who; deleted: (id: string) => void; close: () => void }) {
  const titles = useTitles();
  const [busy, setBusy] = useState(false);
  const info = props.info;

  async function remove() {
    if (!info) return;
    setBusy(true);
    try {
      await api.deleteSession(props.who, info.id);
      props.deleted(info.id);
      props.close();
    } catch (e) {
      toast.error("Not deleted", { description: (e as Error).message });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={info !== undefined} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Delete “{info ? api.title(info, titles.get(info.id)) : ""}”?</DialogTitle>
          <DialogDescription>
            Its conversation, its checkpoints and its secrets are deleted for good. This can't be undone.
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <DialogClose render={<Button type="button" variant="outline" />}>Cancel</DialogClose>
          <Button type="button" variant="destructive" disabled={busy} onClick={remove}>
            {busy ? <Spinner /> : <Trash2Icon />} Delete
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** A session's less used things, in a menu: its id, and stopping it (letting the turn finish, or
 * now) or archiving it for its owner; and starting it, in sight while it's idle and not archived. */
export function SessionMenu(props: {
  id: string;
  info?: SessionInfo;
  who: api.Who;
  changed: () => void;
  rename: () => void;
  openLocally: () => void;
  secrets: () => void;
  remove: () => void;
}) {
  const { info, who, changed } = props;
  const owner = info?.role === "owner";
  const idle = info !== undefined && api.state(info) === "idle";
  return (
    <>
      {owner && idle && !info?.archived && (
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
            {owner && (
              <DropdownMenuItem onClick={props.rename}>
                <PencilIcon /> Rename
              </DropdownMenuItem>
            )}
            {owner && (
              <DropdownMenuItem onClick={props.secrets}>
                <LockKeyholeIcon /> Secrets
              </DropdownMenuItem>
            )}
            <DropdownMenuItem onClick={props.openLocally}>
              <FolderDownIcon /> Open locally
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => navigator.clipboard.writeText(props.id).then(() => toast.success("Session ID copied"))}>
              <CopyIcon /> Copy ID
            </DropdownMenuItem>
            {owner && info && idle && <ArchiveItems info={info} who={who} changed={changed} remove={props.remove} />}
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

/** `list`, the most recently active first: its latest event, or its making (one just made has
 * none yet). */
export function useByActivity(list: SessionInfo[]): SessionInfo[] {
  const activity = useLiveQuery<ActivityRow & Record<string, unknown>>(ACTIVITY_QUERY);
  return useMemo(() => {
    const last = new Map(activity.rows.map((r) => [r.session_id, millis(r.last)]));
    const at = (s: SessionInfo) => Math.max(s.created, last.get(s.id) || 0);
    return [...list].sort((a, b) => at(b) - at(a));
  }, [activity.rows, list]);
}

/** The sessions, by title: a filter, the user's, and those shared with them. */
export function SessionsSidebar(props: {
  sessions: Polled;
  selected: string;
  select: (id: string) => void;
  who: api.Who;
  create: () => void;
  rename: (id: string) => void;
  openLocally: (id: string) => void;
  secrets: (id: string) => void;
  remove: (id: string) => void;
  account: React.ReactNode;
  className?: string;
}) {
  const [filter, setFilter] = useState("");
  const sorted = useByActivity(props.sessions.list);
  const titles = useTitles();
  const wanted = filter.trim().toLowerCase();
  const shown = sorted.filter((s) =>
    [api.title(s, titles.get(s.id)), api.place(s)].some((t) => t.toLowerCase().includes(wanted)),
  );
  const mine = shown.filter((s) => s.role === "owner" && !s.archived);
  const shared = shown.filter((s) => s.role !== "owner" && !s.archived);
  const archived = shown.filter((s) => s.archived);
  const [openArchived, setOpenArchived] = useState(false);
  return (
    <nav aria-label="Sessions" className={cn("flex w-58 shrink-0 flex-col gap-2 bg-panel p-2", props.className)}>
      <label htmlFor="filter" className="sr-only">
        Filter sessions
      </label>
      <Input
        id="filter"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="Filter sessions"
        className="h-7 border-0 bg-secondary px-2 text-xs shadow-none md:text-xs dark:bg-secondary"
      />
      <div className="-mx-1 flex min-h-0 flex-1 flex-col overflow-y-auto px-1">
        {!props.sessions.listed && sorted.length === 0 && (
          <div className="flex flex-col gap-1">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-6 bg-secondary" />
            ))}
          </div>
        )}
        <SessionItems list={mine} titles={titles} {...props} />
        {shared.length > 0 && (
          <>
            <span className="mt-3 mb-1 px-2 text-[11px] text-dim">Shared with you</span>
            <SessionItems list={shared} titles={titles} {...props} />
          </>
        )}
        {archived.length > 0 && (
          <>
            <button
              type="button"
              aria-expanded={openArchived}
              onClick={() => setOpenArchived(!openArchived)}
              className="mt-3 mb-1 flex items-center gap-1 px-2 text-left text-[11px] text-dim hover:text-foreground"
            >
              <ChevronRightIcon className={cn("size-3 transition-transform", openArchived && "rotate-90")} />
              Archived ({archived.length})
            </button>
            {openArchived && <SessionItems list={archived} titles={titles} {...props} />}
          </>
        )}
        <button
          type="button"
          onClick={props.create}
          className="mt-1.5 flex h-7 shrink-0 items-center gap-1.5 rounded-[5px] px-2 text-left text-xs text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <PlusIcon className="size-3.5" /> New session
        </button>
      </div>
      {props.account}
    </nav>
  );
}

function SessionItems(props: {
  list: SessionInfo[];
  titles: Map<string, Named>;
  rename: (id: string) => void;
  openLocally: (id: string) => void;
  secrets: (id: string) => void;
  remove: (id: string) => void;
  selected: string;
  select: (id: string) => void;
  who: api.Who;
  sessions: Polled;
}) {
  return (
    <ul className="flex flex-col gap-px">
      <AnimatePresence initial={false}>
        {props.list.map((s) => {
          const owner = s.role === "owner";
          const st = api.state(s);
          const current = s.id === props.selected;
          const named = props.titles.get(s.id);
          return (
            <motion.li
              key={s.id}
              layout="position"
              initial={{ opacity: 0, x: -6 }}
              animate={{ opacity: 1, x: 0 }}
              exit={{ opacity: 0, x: -6 }}
              transition={{ duration: 0.18 }}
              className="group/item relative"
            >
              <button
                type="button"
                onClick={() => props.select(s.id)}
                title={named ? `${named.title}: ${named.description}` : api.place(s)}
                aria-current={current ? "page" : undefined}
                className={cn(
                  "flex w-full items-center gap-2 rounded-[5px] py-1 pr-7 pl-2 text-left transition-colors",
                  current ? "bg-accent text-strong" : "text-subtle hover:bg-accent/60 hover:text-foreground",
                )}
              >
                <StateDot info={s} />
                <span className="flex min-w-0 flex-col">
                  <span className="truncate">{api.title(s, named)}</span>
                  {named && <span className="truncate text-[11px] text-dim">{api.place(s)}</span>}
                </span>
                <span className="sr-only">, {STATE_NAMES[st]}</span>
              </button>
              <DropdownMenu>
                <DropdownMenuTrigger
                  render={
                    <button
                      type="button"
                      className="absolute top-1/2 right-1 flex size-5 -translate-y-1/2 items-center justify-center rounded text-muted-foreground opacity-0 group-hover/item:opacity-100 hover:bg-secondary hover:text-foreground focus-visible:opacity-100 aria-expanded:opacity-100"
                    />
                  }
                >
                  <EllipsisIcon className="size-3.5" />
                  <span className="sr-only">More</span>
                </DropdownMenuTrigger>
                <DropdownMenuContent side="right" align="start" className="w-48">
                  <DropdownMenuGroup>
                    {owner && st === "idle" && !s.archived && (
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
                  {owner && (
                    <DropdownMenuItem onClick={() => props.rename(s.id)}>
                      <PencilIcon /> Rename
                    </DropdownMenuItem>
                  )}
                  {owner && (
                    <DropdownMenuItem onClick={() => props.secrets(s.id)}>
                      <LockKeyholeIcon /> Secrets
                    </DropdownMenuItem>
                  )}
                  <DropdownMenuItem onClick={() => props.openLocally(s.id)}>
                    <FolderDownIcon /> Open locally
                  </DropdownMenuItem>
                  <DropdownMenuItem onClick={() => navigator.clipboard.writeText(s.id).then(() => toast.success("Session ID copied"))}>
                    <CopyIcon /> Copy ID
                  </DropdownMenuItem>
                  {owner && st === "idle" && (
                    <ArchiveItems info={s} who={props.who} changed={props.sessions.refresh} remove={() => props.remove(s.id)} />
                  )}
                </DropdownMenuContent>
              </DropdownMenu>
            </motion.li>
          );
        })}
      </AnimatePresence>
    </ul>
  );
}

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

const CLAUDE = "claude-sonnet-5";

/** Making a session, `who`'s: a coder on a repository, or a dirigent; paid to Anthropic, or with
 * their ChatGPT plan (`connect` links one). */
export function NewSession(props: {
  open: boolean;
  setOpen: (open: boolean) => void;
  who: api.Who;
  me: Me | null;
  connect: () => void;
  /** For a private repository: give a GITHUB_TOKEN, or connect GitHub's App. */
  addToken: () => void;
  connectGithub: () => void;
  /** Changed when access may have: the repository's is checked again. */
  again?: number;
  made: (id: string) => void;
}) {
  const [part, setPart] = useState<Part>("coder");
  const [git, setGit] = useState("");
  const [ref, setRef] = useState("");
  const [provider, setProvider] = useState<Provider>("anthropic");
  const [models, setModels] = useState<Model[] | null>(null);
  const [model, setModel] = useState(CLAUDE);
  const [effort, setEffort] = useState<Effort | "none">("medium");
  const [start, setStart] = useState(true);
  const [busy, setBusy] = useState(false);
  const plan = linked(props.me);
  const { who } = props;

  // the plan's models, once it's chosen: the first, unless one of them is
  useEffect(() => {
    if (provider !== "chatgpt" || !props.open) return;
    let gone = false;
    api.models(who).then(
      (list) => {
        if (gone) return;
        setModels(list);
        setModel((m) => (list.some((x) => x.slug === m) ? m : (list[0]?.slug ?? "")));
      },
      (err) => !gone && toast.error("The plan's models aren't listed", { description: (err as Error).message }),
    );
    return () => {
      gone = true;
    };
  }, [provider, props.open, who]);

  function choose(p: Provider) {
    setProvider(p);
    if (p === "anthropic") setModel(CLAUDE);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const id = crypto.randomUUID();
    const source =
      part === "coder" && git.trim() ? { git: { url: git.trim(), ...(ref.trim() ? { ref: ref.trim() } : {}) } } : null;
    setBusy(true);
    try {
      await api.put(props.who, id, { model, effort: effort === "none" ? null : effort, source, part, provider }, start);
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
                      <FieldDescription>
                        {api.onGithub(git) ? (
                          <RepoAccess who={who} url={git} addToken={props.addToken} connect={props.connectGithub} again={props.again} />
                        ) : (
                          "Empty: a workspace with no files."
                        )}
                      </FieldDescription>
                    </Field>
                    <Field>
                      <FieldLabel htmlFor="ref">Ref</FieldLabel>
                      <Input id="ref" placeholder="default branch" value={ref} onChange={(e) => setRef(e.target.value)} />
                    </Field>
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
            <Field>
              <FieldLabel>Paid with</FieldLabel>
              <Select items={PROVIDERS} value={provider} onValueChange={(v) => v && choose(v as Provider)}>
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
                {provider === "chatgpt" ? (
                  <Select
                    items={(models ?? []).map((m) => ({ value: m.slug, label: m.display_name }))}
                    value={model}
                    onValueChange={(v) => v && setModel(v as string)}
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
                  <Input id="model" value={model} onChange={(e) => setModel(e.target.value)} required />
                )}
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
            <Button type="submit" disabled={busy || !model}>
              {busy ? <Spinner /> : <PlusIcon />} Make it
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
