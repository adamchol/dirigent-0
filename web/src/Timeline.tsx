// A session's timeline (events.ts) as a conversation: the user's messages as bubbles, the model's
// text as Markdown, its thinking and tool calls as a trace, turns and the player's doings as
// markers. Entries arriving after the first render come in with a short animation.

import { useState } from "react";
import { motion } from "motion/react";
import {
  BanIcon,
  BrainIcon,
  CameraIcon,
  CheckIcon,
  ClockIcon,
  FolderGitIcon,
  HistoryIcon,
  InfoIcon,
  KeyRoundIcon,
  MinimizeIcon,
  PlugIcon,
  RefreshCwIcon,
  TriangleAlertIcon,
  UnplugIcon,
  XIcon,
} from "lucide-react";

import { Bubble, BubbleContent } from "@/components/ui/bubble";
import { Button } from "@/components/ui/button";
import { Marker, MarkerContent, MarkerIcon } from "@/components/ui/marker";
import { Message, MessageContent, MessageHeader } from "@/components/ui/message";
import { MessageScrollerItem } from "@/components/ui/message-scroller";
import { Spinner } from "@/components/ui/spinner";
import {
  Trace,
  TraceCollapsible,
  TraceCollapsibleContent,
  TraceCollapsibleTrigger,
  TraceDescription,
  TraceIcon,
  TraceLine,
  TraceTitle,
} from "@/components/ui/trace";
import type { Item, Streamed, Usage } from "./events.ts";
import type { Request } from "./Secrets.tsx";
import { Markdown } from "./Markdown.tsx";
import { describe, ToolDetail } from "./Tools.tsx";

type Step = Extract<Item, { t: "thinking" | "tool" }>;

/** What the timeline shows, its model's thinking and tool calls in a row together. */
type Block = Exclude<Item, Step> | { t: "steps"; key: string; steps: Step[] };

function blocks(items: readonly Item[]): Block[] {
  const out: Block[] = [];
  for (const item of items) {
    if (item.t === "thinking" || item.t === "tool") {
      const last = out[out.length - 1];
      if (last?.t === "steps") last.steps.push(item);
      else out.push({ t: "steps", key: item.key, steps: [item] });
    } else out.push(item);
  }
  return out;
}

const ARRIVE = { opacity: 0, y: 10, filter: "blur(2px)" };
const SETTLED = { opacity: 1, y: 0, filter: "blur(0px)" };
const EASE = [0.23, 1, 0.32, 1] as const;

export function Timeline(props: {
  items: readonly Item[];
  /** The turn running, if one is. */
  running: boolean;
  /** The user's own messages chlive doesn't have yet. */
  sending: ReadonlySet<string>;
  /** Who looks: their messages aren't labelled. */
  me: string;
  /** Keys shown at first: these don't animate in. */
  first: ReadonlySet<string>;
  /** The reply being written, as its deltas have it so far. */
  streaming: readonly Streamed[];
  /** Answer a dirigent's request for a secret: its owner's only. */
  answer?: (request: Request, provided: boolean) => void;
}) {
  const list = blocks(props.items);
  const last = list[list.length - 1];
  // the model's working when the turn's last shown thing isn't still running
  const thinking =
    props.running &&
    props.streaming.length === 0 &&
    !(last?.t === "steps" && last.steps.some((s) => s.t === "tool" && !s.result));
  return (
    <>
      {list.map((block) => (
        <MessageScrollerItem key={block.key} scrollAnchor={block.t === "user"}>
          <motion.div
            initial={props.first.has(block.key) ? false : ARRIVE}
            animate={SETTLED}
            transition={{ duration: 0.35, ease: EASE }}
          >
            <Entry block={block} running={props.running} sending={props.sending} me={props.me} answer={props.answer} />
          </motion.div>
        </MessageScrollerItem>
      ))}
      {props.streaming.length > 0 && (
        <MessageScrollerItem key="streaming">
          <motion.div initial={{ opacity: 0, y: 6 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.25, ease: EASE }}>
            <Writing blocks={props.streaming} />
          </motion.div>
        </MessageScrollerItem>
      )}
      {thinking && (
        <MessageScrollerItem key="working">
          <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.15 }}>
            <Trace>
              <TraceLine status="running">
                <TraceIcon>
                  <Spinner />
                </TraceIcon>
                <TraceTitle>Working…</TraceTitle>
              </TraceLine>
            </Trace>
          </motion.div>
        </MessageScrollerItem>
      )}
    </>
  );
}

/** The reply being written: its text as it comes, its thinking's latest lines, the tool calls it
 * forms; replaced by the reply once it's whole. */
function Writing({ blocks }: { blocks: readonly Streamed[] }) {
  // its thinking and tool calls in a row together, as the timeline's steps
  const groups: Streamed[][] = [];
  for (const b of blocks) {
    const last = groups[groups.length - 1];
    if (last && b.t !== "text" && last[0]!.t !== "text") last.push(b);
    else groups.push([b]);
  }
  return (
    <div className="flex flex-col gap-6">
      {groups.map((group) =>
        group[0]!.t === "text" ? (
          <Message key={group[0]!.key}>
            <MessageContent>
              <Bubble variant="ghost" className="w-full">
                <BubbleContent className="w-full">
                  <Markdown text={group[0]!.text} />
                </BubbleContent>
              </Bubble>
            </MessageContent>
          </Message>
        ) : (
          <Trace key={group[0]!.key}>
            {group.map((b) =>
              b.t === "thinking" ? (
                <TraceCollapsible key={b.key} defaultOpen>
                  <TraceCollapsibleTrigger>
                    <TraceLine status="running">
                      <TraceIcon>
                        <BrainIcon />
                      </TraceIcon>
                      <TraceTitle>Thinking</TraceTitle>
                    </TraceLine>
                  </TraceCollapsibleTrigger>
                  <TraceCollapsibleContent>
                    {/* its latest lines, the earlier fading out above */}
                    <div className="flex max-h-32 flex-col-reverse overflow-hidden mask-t-from-60%">
                      <p className="text-sm whitespace-pre-wrap">{b.text}</p>
                    </div>
                  </TraceCollapsibleContent>
                </TraceCollapsible>
              ) : b.t === "tool" ? (
                <TraceLine key={b.key} status="running">
                  <TraceIcon>
                    <Spinner />
                  </TraceIcon>
                  <TraceTitle>{describe(b.name, b.input).title}</TraceTitle>
                </TraceLine>
              ) : null,
            )}
          </Trace>
        ),
      )}
    </div>
  );
}

function Entry(props: {
  block: Block;
  running: boolean;
  sending: ReadonlySet<string>;
  me: string;
  answer?: (request: Request, provided: boolean) => void;
}) {
  const b = props.block;
  switch (b.t) {
    case "user": {
      const sending = props.sending.has(b.key);
      return (
        <Message>
          <MessageContent className="gap-1">
            <MessageHeader className="gap-1 px-0 font-normal text-dim">
              {b.via ? (
                <span>
                  Dirigent{" "}
                  <a className="font-mono underline-offset-3 hover:underline" href={`#${b.via}`}>
                    {b.via.slice(0, 8)}
                  </a>
                </span>
              ) : b.author === props.me ? (
                "You"
              ) : (
                b.author.replace(/^user:/, "")
              )}
              <span>·</span>
              {sending ? (
                <span className="flex items-center gap-1">
                  <Spinner className="size-3" /> Sending
                </span>
              ) : b.queued ? (
                <span className="flex items-center gap-1">
                  <ClockIcon className="size-3" /> Queued · {time(b.ts)}
                </span>
              ) : (
                time(b.ts)
              )}
            </MessageHeader>
            <Bubble variant="muted" className="w-full max-w-full">
              <BubbleContent className="w-full rounded-md px-3 py-2 whitespace-pre-wrap text-strong">{b.text}</BubbleContent>
            </Bubble>
          </MessageContent>
        </Message>
      );
    }
    case "update":
      return (
        <Message>
          <MessageContent>
            <MessageHeader className="gap-1">
              <HistoryIcon className="size-3.5" />
              Session
              <a className="font-mono underline-offset-3 hover:underline" href={`#${b.session}`}>
                {b.session.slice(0, 8)}
              </a>
              · {b.what}
              {b.queued && <span className="text-muted-foreground/70"> · queued</span>}
            </MessageHeader>
            {b.reply && (
              <Bubble variant="tinted">
                <BubbleContent>
                  <Markdown text={b.reply} />
                </BubbleContent>
              </Bubble>
            )}
          </MessageContent>
        </Message>
      );
    case "text":
      return (
        <Message>
          <MessageContent>
            <Bubble variant="ghost" className="w-full">
              <BubbleContent className="w-full">
                <Markdown text={b.text} />
              </BubbleContent>
            </Bubble>
          </MessageContent>
        </Message>
      );
    case "steps":
      return (
        <Trace>
          {b.steps.map((s) => (s.t === "thinking" ? <Thought key={s.key} text={s.text} /> : <Tool key={s.key} step={s} running={props.running} />))}
        </Trace>
      );
    case "turn":
      return <TurnMarker reason={b.reason} interrupted={b.interrupted} usage={b.usage} />;
    case "request":
      return <SecretRequest request={b} answer={props.answer} />;
    case "note":
      return <Note kind={b.kind} tone={b.tone} text={b.text} detail={b.detail} />;
  }
}

function Thought({ text }: { text: string }) {
  return (
    <TraceCollapsible>
      <TraceCollapsibleTrigger>
        <TraceLine>
          <TraceIcon>
            <BrainIcon />
          </TraceIcon>
          <TraceTitle>Thought</TraceTitle>
          <TraceDescription>{text.split(/\s+/).length} words</TraceDescription>
        </TraceLine>
      </TraceCollapsibleTrigger>
      <TraceCollapsibleContent>
        <p className="text-sm whitespace-pre-wrap">{text}</p>
      </TraceCollapsibleContent>
    </TraceCollapsible>
  );
}

function Tool({ step, running }: { step: Extract<Step, { t: "tool" }>; running: boolean }) {
  const view = describe(step.name, step.input);
  const [open, setOpen] = useState(false);
  const status = step.result?.isError ? "error" : !step.result && running ? "running" : undefined;
  return (
    <TraceCollapsible open={open} onOpenChange={setOpen}>
      <TraceCollapsibleTrigger>
        <TraceLine status={status}>
          <TraceIcon>{status === "running" ? <Spinner /> : view.icon}</TraceIcon>
          <TraceTitle>{view.title}</TraceTitle>
          {status === "error" ? (
            <TraceDescription>failed</TraceDescription>
          ) : !step.result && !running ? (
            <TraceDescription>cancelled</TraceDescription>
          ) : (
            view.meta && <TraceDescription>{view.meta}</TraceDescription>
          )}
        </TraceLine>
      </TraceCollapsibleTrigger>
      <TraceCollapsibleContent className="text-foreground">
        {/* rendered only opened: highlighting a long file costs */}
        {open && <ToolDetail name={step.name} input={step.input} result={step.result} />}
      </TraceCollapsibleContent>
    </TraceCollapsible>
  );
}

function TurnMarker(props: { reason: string; interrupted: boolean; usage: Usage }) {
  const failed = props.reason === "failed";
  const icon = props.interrupted || props.reason === "aborted" ? <BanIcon /> : failed ? <XIcon /> : <CheckIcon />;
  const what = props.interrupted ? "Interrupted" : props.reason === "completed" ? "Done" : props.reason === "aborted" ? "Aborted" : "Failed";
  const u = props.usage;
  return (
    <Marker variant="separator" className={failed ? "text-destructive" : undefined}>
      <MarkerIcon>{icon}</MarkerIcon>
      <MarkerContent className="text-xs">
        {what} · {tokens(u.input + u.cache_read + u.cache_write)} in
        {u.cache_read > 0 && ` (${tokens(u.cache_read)} cached)`} · {tokens(u.output)} out
      </MarkerContent>
    </Marker>
  );
}

const NOTE_ICONS: Record<string, React.ReactNode> = {
  player_attached: <PlugIcon />,
  player_detached: <UnplugIcon />,
  workspace_ready: <FolderGitIcon />,
  workspace_restored: <RefreshCwIcon />,
  checkpoint: <CameraIcon />,
  compacted: <MinimizeIcon />,
};

function Note(props: { kind: string; tone: "dim" | "error"; text: string; detail?: string }) {
  const icon = props.tone === "error" ? <TriangleAlertIcon /> : (NOTE_ICONS[props.kind] ?? <InfoIcon />);
  const tone = props.tone === "error" ? "text-destructive" : "text-muted-foreground/80";
  if (!props.detail)
    return (
      <Marker className={`justify-center text-xs ${tone}`}>
        <MarkerIcon className="size-3.5 [&_svg]:size-3.5">{icon}</MarkerIcon>
        <MarkerContent>{props.text}</MarkerContent>
      </Marker>
    );
  return (
    <Trace className="items-center">
      <TraceCollapsible className="items-center">
        <TraceCollapsibleTrigger className={`mx-auto text-xs ${tone}`}>
          <TraceLine status={props.tone === "error" ? "error" : undefined} className="text-xs">
            <TraceIcon className="size-3.5 [&_svg]:size-3.5">{icon}</TraceIcon>
            <TraceTitle>{props.text}</TraceTitle>
          </TraceLine>
        </TraceCollapsibleTrigger>
        <TraceCollapsibleContent className="w-full max-w-2xl self-center">
          <div className="max-h-80 overflow-auto text-xs whitespace-pre-wrap">{props.detail}</div>
        </TraceCollapsibleContent>
      </TraceCollapsible>
    </Trace>
  );
}

export function tokens(n: number): string {
  return n >= 10_000 ? `${Math.round(n / 1000)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`;
}

/** A row's timestamp (the client's parsed one, or ClickHouse's DateTime64 text, UTC) as local
 * time. */
function time(ts: string | Date): string {
  const d = ts instanceof Date ? ts : new Date(ts.includes("T") ? ts : `${ts.replace(" ", "T")}Z`);
  return isNaN(d.getTime()) ? String(ts) : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** A dirigent's request for a secret: what it's for, and, to the owner, providing or declining
 * it, until they have (or control refused the request). */
function SecretRequest(props: { request: Request; answer?: (request: Request, provided: boolean) => void }) {
  const r = props.request;
  const refused = r.result?.isError;
  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-panel px-4 py-3 text-[13px]">
      <div className="flex items-center gap-2">
        <KeyRoundIcon className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0">
          Session{" "}
          <a className="font-mono underline-offset-3 hover:underline" href={`#${r.session}`}>
            {r.session.slice(0, 8)}
          </a>{" "}
          needs <code className="font-mono text-[12px] text-strong">{r.name}</code>
        </span>
      </div>
      {r.why && <p className="text-muted-foreground">{r.why}</p>}
      {refused ? (
        <p className="text-destructive">{r.result!.content}</p>
      ) : r.answer ? (
        <p className="flex items-center gap-1.5 text-muted-foreground">
          {r.answer === "provided" ? <CheckIcon className="size-3.5" /> : <XIcon className="size-3.5" />}
          {r.answer === "provided" ? "Provided" : "Declined"}
        </p>
      ) : props.answer ? (
        <div className="flex gap-2">
          <Button size="sm" onClick={() => props.answer!(r, true)}>
            <KeyRoundIcon /> Provide
          </Button>
          <Button size="sm" variant="ghost" onClick={() => props.answer!(r, false)}>
            Decline
          </Button>
        </div>
      ) : (
        <p className="text-muted-foreground">Waiting for its owner.</p>
      )}
    </div>
  );
}
