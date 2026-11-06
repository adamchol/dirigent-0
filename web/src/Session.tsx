// A session's log, live through chlive as the client's user, as a conversation; and what that
// user writes into it: messages, and interrupting the running turn. Its owner also starts and
// stops it, and says who else may read it.

import { useEffect, useMemo, useRef, useState } from "react";
import type { LiveClient } from "@chlive/client";
import { useLiveQuery } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import { ArrowUpIcon, EyeIcon, LockIcon, MessageSquareDashedIcon, SquareIcon, UsersIcon, XIcon } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertAction, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput, InputGroupTextarea } from "@/components/ui/input-group";
import { Kbd } from "@/components/ui/kbd";
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from "@/components/ui/message-scroller";
import { Popover, PopoverContent, PopoverDescription, PopoverHeader, PopoverTitle, PopoverTrigger } from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "./api.ts";
import type { SessionInfo } from "./api.ts";
import { callStart, DELTAS_QUERY, LOG_QUERY, streamed, timeline, type DeltaRow, type EventRow } from "./events.ts";
import { SessionMenu, StateBadge } from "./Sessions.tsx";
import { Timeline } from "./Timeline.tsx";

export function Session(props: {
  id: string;
  client: LiveClient;
  who: api.Who;
  /** The session as control shows it to the user; none, it's none of theirs (once `listed`). */
  info?: SessionInfo;
  /** Whether control's list came. */
  listed: boolean;
  /** Look as another user (`declared` auth only). */
  become?: (user: string) => void;
  /** The session changed (started, stopped, its viewers). */
  changed: () => void;
}) {
  // only its owner writes to it; chlive shows anyone else without access an empty log
  const owner = props.info?.role === "owner";
  const log = useLiveQuery<EventRow & Record<string, unknown>>(LOG_QUERY, { params: { session: props.id } });
  const { items, openTurn } = useMemo(() => timeline(log.rows), [log.rows]);
  // the user's own messages, shown before chlive has them
  const sending = useMemo(() => new Set([...(log.pending ?? [])].map((r) => r.id)), [log.pending]);
  // the reply being written: the running model call's deltas, while a turn is open
  const after = useMemo(() => callStart(log.rows), [log.rows]);
  const deltas = useLiveQuery<DeltaRow & Record<string, unknown>>(DELTAS_QUERY, {
    params: { session: props.id, after },
    enabled: openTurn !== undefined,
  });
  const writing = useMemo(() => (openTurn !== undefined ? streamed(deltas.rows) : []), [deltas.rows, openTurn]);

  // what's there when the log first comes in isn't animated in
  const first = useRef<Set<string> | null>(null);
  if (first.current === null && log.status !== "loading") first.current = new Set(items.map((i) => i.key));
  // nor is a reply that was being written: it's there already
  const wasWriting = useRef(false);
  if (wasWriting.current) for (const i of items) first.current?.add(i.key);
  wasWriting.current = writing.length > 0;

  async function write(kind: string, data: unknown) {
    try {
      await props.client.insert("session_events", {
        session_id: props.id,
        id: crypto.randomUUID(),
        kind,
        data: JSON.stringify(data),
      });
    } catch (e) {
      toast.error("Not sent", { description: (e as Error).message });
    }
  }

  return (
    <div className="flex h-svh min-w-0 flex-col">
      <header className="flex h-14 shrink-0 items-center gap-2 border-b px-3">
        <SidebarTrigger />
        <Separator orientation="vertical" className="mx-1 h-4 data-vertical:self-center" />
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <h1 className="truncate text-sm font-medium">{props.info ? api.title(props.info) : "Session"}</h1>
          {props.info && <StateBadge info={props.info} />}
          {props.info && (
            <Badge variant="outline" className="hidden font-normal text-muted-foreground md:inline-flex">
              {props.info.spec.model}
              {props.info.spec.effort ? ` · ${props.info.spec.effort}` : ""}
            </Badge>
          )}
          <LiveDot status={log.status} error={log.error?.message} />
        </div>
        {props.info && owner && <Viewers info={props.info} who={props.who} changed={props.changed} />}
        <SessionMenu id={props.id} info={props.info} who={props.who} changed={props.changed} />
      </header>

      <AnimatePresence initial={false}>
        {props.info && !owner && (
          <motion.div initial={{ height: 0, opacity: 0 }} animate={{ height: "auto", opacity: 1 }} exit={{ height: 0, opacity: 0 }}>
            <div className="mx-auto w-full max-w-3xl px-4 pt-4">
              <Alert>
                <EyeIcon />
                <AlertTitle>Viewing {props.info.owner}'s session</AlertTitle>
                <AlertDescription>Only its owner writes to it, starts and stops it.</AlertDescription>
                {props.become && (
                  <AlertAction>
                    <Button size="sm" variant="outline" onClick={() => props.become!(props.info!.owner)}>
                      Look as {props.info.owner}
                    </Button>
                  </AlertAction>
                )}
              </Alert>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
      {!props.info && props.listed && (
        <div className="mx-auto w-full max-w-3xl px-4 pt-4">
          <Alert variant="destructive">
            <LockIcon />
            <AlertTitle>No access</AlertTitle>
            <AlertDescription>{props.who.user} may not read this session.</AlertDescription>
          </Alert>
        </div>
      )}

      <MessageScrollerProvider>
        <MessageScroller className="flex-1">
          <MessageScrollerViewport>
            <MessageScrollerContent className="mx-auto w-full max-w-3xl px-4 py-6">
              {log.status === "loading" ? (
                <Loading />
              ) : log.status === "error" ? (
                <Alert variant="destructive">
                  <LockIcon />
                  <AlertTitle>No log</AlertTitle>
                  <AlertDescription>{log.error?.message}</AlertDescription>
                </Alert>
              ) : items.length === 0 ? (
                <Empty className="my-auto">
                  <EmptyHeader>
                    <EmptyMedia variant="icon">
                      <MessageSquareDashedIcon />
                    </EmptyMedia>
                    <EmptyTitle>Nothing yet</EmptyTitle>
                    <EmptyDescription>
                      {owner ? "Say what to do: the session's player takes it as its first turn." : "The log is empty."}
                    </EmptyDescription>
                  </EmptyHeader>
                </Empty>
              ) : (
                <Timeline items={items} running={openTurn !== undefined} sending={sending} me={props.who.user} first={first.current ?? new Set()} streaming={writing} />
              )}
            </MessageScrollerContent>
          </MessageScrollerViewport>
          <MessageScrollerButton />
        </MessageScroller>
      </MessageScrollerProvider>

      <Composer
        disabled={!owner}
        who={props.who.user}
        running={openTurn !== undefined}
        send={(text) => write("user_message", { text })}
        interrupt={() => openTurn && write("user_interrupt", { turn: openTurn })}
      />
    </div>
  );
}

function Loading() {
  return (
    <div className="flex flex-col gap-6">
      <Skeleton className="ml-auto h-10 w-2/5 rounded-xl" />
      <div className="flex gap-2">
        <Skeleton className="size-8 shrink-0 rounded-full" />
        <div className="flex flex-1 flex-col gap-2">
          <Skeleton className="h-4 w-4/5" />
          <Skeleton className="h-4 w-3/5" />
        </div>
      </div>
      <Skeleton className="ml-10 h-4 w-1/3" />
    </div>
  );
}

/** chlive's connection for this log: live, catching up, or failed. */
function LiveDot(props: { status: string; error?: string }) {
  const color =
    props.status === "live" ? "bg-emerald-500" : props.status === "error" ? "bg-destructive" : "bg-amber-500";
  const label =
    props.status === "live" ? "Live" : props.status === "error" ? `Failed: ${props.error}` : "Connecting…";
  return (
    <Tooltip>
      <TooltipTrigger render={<span className="relative ml-1 flex size-2" />}>
        {props.status !== "live" && props.status !== "error" && (
          <span className={`absolute inline-flex size-full animate-ping rounded-full opacity-60 ${color}`} />
        )}
        <span className={`relative inline-flex size-2 rounded-full ${color}`} />
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

function Composer(props: {
  disabled: boolean;
  who: string;
  running: boolean;
  send: (text: string) => void;
  interrupt: () => void;
}) {
  const [draft, setDraft] = useState("");
  const box = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    if (!props.disabled) box.current?.focus();
  }, [props.disabled]);

  function submit(e?: React.FormEvent) {
    e?.preventDefault();
    const text = draft.trim();
    if (!text || props.disabled) return;
    setDraft("");
    props.send(text);
  }

  return (
    <form onSubmit={submit} className="mx-auto w-full max-w-3xl shrink-0 px-4 pb-4">
      <InputGroup className="rounded-2xl bg-background shadow-sm has-[textarea:focus-visible]:shadow-md">
        <InputGroupTextarea
          ref={box}
          value={draft}
          disabled={props.disabled}
          rows={1}
          className="max-h-48 min-h-12 field-sizing-content"
          placeholder={props.disabled ? "Only the session's owner writes to it" : props.running ? "Steer the running turn…" : `Message as ${props.who}`}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) submit(e);
          }}
        />
        <InputGroupAddon align="block-end">
          <span className="hidden items-center gap-1 text-xs text-muted-foreground sm:flex">
            <Kbd>Enter</Kbd> to send · <Kbd>Shift</Kbd>+<Kbd>Enter</Kbd> new line
          </span>
          <div className="ml-auto flex items-center gap-1">
            <AnimatePresence initial={false}>
              {props.running && !props.disabled && (
                <motion.div key="interrupt" initial={{ opacity: 0, scale: 0.8 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0, scale: 0.8 }}>
                  <Tooltip>
                    <TooltipTrigger render={<InputGroupButton variant="outline" size="icon-sm" className="rounded-full" onClick={props.interrupt} />}>
                      <SquareIcon className="fill-current" />
                      <span className="sr-only">Interrupt</span>
                    </TooltipTrigger>
                    <TooltipContent>Interrupt the turn</TooltipContent>
                  </Tooltip>
                </motion.div>
              )}
            </AnimatePresence>
            <motion.div whileTap={{ scale: 0.9 }}>
              <InputGroupButton type="submit" variant="default" size="icon-sm" className="rounded-full" disabled={props.disabled || !draft.trim()}>
                <ArrowUpIcon />
                <span className="sr-only">Send</span>
              </InputGroupButton>
            </motion.div>
          </div>
        </InputGroupAddon>
      </InputGroup>
    </form>
  );
}

/** The session's viewers, which its owner adds and removes. */
function Viewers(props: { info: SessionInfo; who: api.Who; changed: () => void }) {
  const [draft, setDraft] = useState("");
  const act = async (f: () => Promise<void>) => {
    try {
      await f();
    } catch (e) {
      toast.error("Viewers not changed", { description: (e as Error).message });
    }
    props.changed();
  };
  function add(e: React.FormEvent) {
    e.preventDefault();
    const viewer = draft.trim();
    if (!viewer) return;
    setDraft("");
    act(() => api.addViewer(props.who, props.info.id, viewer));
  }
  const n = props.info.viewers.length;
  return (
    <Popover>
      <PopoverTrigger render={<Button variant="ghost" size="sm" />}>
        <UsersIcon />
        {n > 0 && <span className="tabular-nums">{n}</span>}
        <span className="sr-only">Viewers</span>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80">
        <PopoverHeader>
          <PopoverTitle>Viewers</PopoverTitle>
          <PopoverDescription>They read the session; only you write to it.</PopoverDescription>
        </PopoverHeader>
        <div className="flex flex-wrap gap-1.5">
          <AnimatePresence initial={false}>
            {props.info.viewers.map((v) => (
              <motion.span key={v} layout initial={{ opacity: 0, scale: 0.8 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0, scale: 0.8 }}>
                <Badge variant="secondary" className="gap-1 pr-1">
                  {v}
                  <button
                    type="button"
                    className="rounded-full p-0.5 hover:bg-foreground/10"
                    onClick={() => act(() => api.removeViewer(props.who, props.info.id, v))}
                  >
                    <XIcon className="size-3" />
                    <span className="sr-only">Remove {v}</span>
                  </button>
                </Badge>
              </motion.span>
            ))}
          </AnimatePresence>
          {n === 0 && <span className="text-sm text-muted-foreground">No one else.</span>}
        </div>
        <form onSubmit={add}>
          <InputGroup>
            <InputGroupInput placeholder="user:someone@example.com" value={draft} onChange={(e) => setDraft(e.target.value)} spellCheck={false} />
            <InputGroupAddon align="inline-end">
              <InputGroupButton type="submit" disabled={!draft.trim()}>
                Add
              </InputGroupButton>
            </InputGroupAddon>
          </InputGroup>
        </form>
      </PopoverContent>
    </Popover>
  );
}
