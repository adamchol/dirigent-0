// A session's log, live through chlive as the client's user, as a conversation; and what that
// user writes into it: messages, and interrupting the running turn. Its owner also says who else
// may read it (`Viewers`, in the top bar). What the status bar shows of it goes up by `report`.

import { useEffect, useMemo, useRef, useState } from "react";
import type { LiveClient } from "@chlive/client";
import { useLiveQuery } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import { EyeIcon, LockIcon, MessageSquareDashedIcon, SquareIcon, UnlinkIcon, UsersIcon, XIcon } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertAction, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput, InputGroupTextarea } from "@/components/ui/input-group";
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from "@/components/ui/message-scroller";
import { Popover, PopoverContent, PopoverDescription, PopoverHeader, PopoverTitle, PopoverTrigger } from "@/components/ui/popover";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "./api.ts";
import type { SessionInfo } from "./api.ts";
import { callStart, contextSize, DELTAS_QUERY, LOG_QUERY, streamed, timeline, type DeltaRow, type EventRow } from "./events.ts";
import { linked } from "./Link.tsx";
import { Timeline } from "./Timeline.tsx";

/** What the status bar shows of the session open. */
export interface LiveStatus {
  /** chlive's for its log: `live`, `loading`, `error`, … */
  log: string;
  error?: string;
  /** A turn is running. */
  running: boolean;
  /** The model's context at its last call, in tokens. */
  context: number;
}

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
  report: (status: LiveStatus) => void;
  /** The user, and the ChatGPT plan they linked; and how they link it again. */
  me: api.Me | null;
  connect: () => void;
}) {
  // only its owner writes to it; chlive shows anyone else without access an empty log
  const owner = props.info?.role === "owner";
  const log = useLiveQuery<EventRow & Record<string, unknown>>(LOG_QUERY, { params: { session: props.id } });
  const { items, openTurn } = useMemo(() => timeline(log.rows), [log.rows]);
  const context = useMemo(() => contextSize(log.rows), [log.rows]);
  // the user's own messages, shown before chlive has them
  const sending = useMemo(() => new Set([...(log.pending ?? [])].map((r) => r.id)), [log.pending]);
  // the reply being written: the running model call's deltas, while a turn is open
  const after = useMemo(() => callStart(log.rows), [log.rows]);
  const deltas = useLiveQuery<DeltaRow & Record<string, unknown>>(DELTAS_QUERY, {
    params: { session: props.id, after },
    enabled: openTurn !== undefined,
  });
  const writing = useMemo(() => (openTurn !== undefined ? streamed(deltas.rows) : []), [deltas.rows, openTurn]);

  const { report } = props;
  const running = openTurn !== undefined;
  useEffect(() => {
    report({ log: log.status, error: log.error?.message, running, context });
  }, [report, log.status, log.error?.message, running, context]);

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
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      <AnimatePresence initial={false}>
        {props.info && !owner && (
          <motion.div initial={{ height: 0, opacity: 0 }} animate={{ height: "auto", opacity: 1 }} exit={{ height: 0, opacity: 0 }}>
            <div className="mx-auto w-full max-w-185 px-6 pt-5">
              <Alert className="border-0 bg-panel">
                <EyeIcon />
                <AlertTitle>Viewing {props.info.owner}'s session</AlertTitle>
                <AlertDescription>Only its owner writes to it, starts and stops it.</AlertDescription>
                {props.become && (
                  <AlertAction>
                    <Button size="sm" variant="secondary" onClick={() => props.become!(props.info!.owner)}>
                      Look as {props.info.owner}
                    </Button>
                  </AlertAction>
                )}
              </Alert>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
      {owner && props.info && api.provider(props.info) === "chatgpt" && props.me && !linked(props.me) && (
        <div className="mx-auto w-full max-w-185 px-6 pt-5">
          <Alert variant="destructive" className="border-0 bg-panel">
            <UnlinkIcon />
            <AlertTitle>{props.me.chatgpt ? "The ChatGPT link is broken" : "No ChatGPT plan is linked"}</AlertTitle>
            <AlertDescription>
              {props.me.chatgpt?.broken ?? "This session runs on your ChatGPT plan."} Its turns fail until it's linked again.
            </AlertDescription>
            <AlertAction>
              <Button size="sm" variant="secondary" onClick={props.connect}>
                Reconnect
              </Button>
            </AlertAction>
          </Alert>
        </div>
      )}

      {!props.info && props.listed && (
        <div className="mx-auto w-full max-w-185 px-6 pt-5">
          <Alert variant="destructive" className="border-0 bg-panel">
            <LockIcon />
            <AlertTitle>No access</AlertTitle>
            <AlertDescription>{props.who.user} may not read this session.</AlertDescription>
          </Alert>
        </div>
      )}

      <MessageScrollerProvider>
        <MessageScroller className="flex-1">
          <MessageScrollerViewport>
            <MessageScrollerContent className="mx-auto w-full max-w-185 gap-5 px-6 py-6">
              {log.status === "loading" ? (
                <Loading />
              ) : log.status === "error" ? (
                <Alert variant="destructive" className="border-0 bg-panel">
                  <LockIcon />
                  <AlertTitle>No log</AlertTitle>
                  <AlertDescription>{log.error?.message}</AlertDescription>
                </Alert>
              ) : items.length === 0 ? (
                <Empty className="my-auto">
                  <EmptyHeader>
                    <EmptyMedia variant="icon" className="bg-panel">
                      <MessageSquareDashedIcon />
                    </EmptyMedia>
                    <EmptyTitle className="text-strong">Nothing yet</EmptyTitle>
                    <EmptyDescription>
                      {owner ? "Say what to do: the session's player takes it as its first turn." : "The log is empty."}
                    </EmptyDescription>
                  </EmptyHeader>
                </Empty>
              ) : (
                <Timeline
                  items={items}
                  running={running}
                  sending={sending}
                  me={props.who.user}
                  first={first.current ?? new Set()}
                  streaming={writing}
                />
              )}
            </MessageScrollerContent>
          </MessageScrollerViewport>
          <MessageScrollerButton />
        </MessageScroller>
      </MessageScrollerProvider>

      <Composer
        disabled={!owner}
        running={running}
        send={(text) => write("user_message", { text })}
        interrupt={() => openTurn && write("user_interrupt", { turn: openTurn })}
      />
    </div>
  );
}

function Loading() {
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col gap-1.5">
        <Skeleton className="h-3 w-20 bg-panel" />
        <Skeleton className="h-10 w-full bg-panel" />
      </div>
      <Skeleton className="h-3 w-1/3 bg-panel" />
      <Skeleton className="h-3 w-2/5 bg-panel" />
      <div className="flex flex-col gap-2">
        <Skeleton className="h-4 w-4/5 bg-panel" />
        <Skeleton className="h-4 w-3/5 bg-panel" />
      </div>
    </div>
  );
}

function Composer(props: { disabled: boolean; running: boolean; send: (text: string) => void; interrupt: () => void }) {
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
    <form onSubmit={submit} className="mx-auto w-full max-w-185 shrink-0 px-6 pb-4">
      <InputGroup className="h-auto items-end rounded-lg border-border bg-panel shadow-none dark:bg-panel">
        <label htmlFor="message" className="sr-only">
          Message
        </label>
        <InputGroupTextarea
          id="message"
          ref={box}
          value={draft}
          disabled={props.disabled}
          rows={2}
          className="max-h-48 min-h-14 py-2.5 text-[14px] text-strong field-sizing-content"
          placeholder={
            props.disabled ? "Only the session's owner writes to it" : props.running ? "Steer the running turn…" : "Message the session"
          }
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) submit(e);
          }}
        />
        <InputGroupAddon align="inline-end" className="gap-1.5 self-end pb-2">
          <AnimatePresence initial={false}>
            {props.running && !props.disabled && (
              <motion.div key="interrupt" initial={{ opacity: 0, scale: 0.85 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0, scale: 0.85 }}>
                <Tooltip>
                  <TooltipTrigger
                    render={<InputGroupButton variant="secondary" size="sm" className="h-7 px-2.5" onClick={props.interrupt} />}
                  >
                    <SquareIcon className="size-3 fill-current" /> Stop
                  </TooltipTrigger>
                  <TooltipContent>Interrupt the turn</TooltipContent>
                </Tooltip>
              </motion.div>
            )}
          </AnimatePresence>
          <motion.div whileTap={{ scale: 0.94 }}>
            <InputGroupButton type="submit" variant="default" size="sm" className="h-7 px-3 font-medium" disabled={props.disabled || !draft.trim()}>
              Send ⏎
            </InputGroupButton>
          </motion.div>
        </InputGroupAddon>
      </InputGroup>
    </form>
  );
}

/** The session's viewers, which its owner adds and removes. */
export function Viewers(props: { info: SessionInfo; who: api.Who; changed: () => void }) {
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
      <PopoverTrigger render={<Button variant="ghost" size="sm" className="h-7 gap-1.5 px-2 text-muted-foreground" />}>
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
