// A tool call as the timeline shows it: an icon and a line saying what it did (player/src/tools,
// and the dirigent's in player/src/dirigent.rs), and, opened, its input and result in the form
// that suits it: a command and its output, a file, a diff.

import type { ReactNode } from "react";
import {
  FileTextIcon,
  FilePenIcon,
  FilePlusIcon,
  FolderSearchIcon,
  ListIcon,
  PlayIcon,
  SearchIcon,
  SendIcon,
  SparklesIcon,
  SquareIcon,
  TerminalIcon,
  WrenchIcon,
} from "lucide-react";

import { Code, Diff, languageOf } from "./Code.tsx";

type Input = Record<string, unknown>;

const str = (v: unknown): string => (typeof v === "string" ? v : "");

/** A path's last parts, short enough for a line. */
function short(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join("/")}` : path;
}

function Mono({ children }: { children: ReactNode }) {
  return <code className="rounded bg-muted px-1 py-px font-mono text-[0.85em] text-foreground/80">{children}</code>;
}

/** The first line of `s`, cut short. */
function line(s: string, max = 72): string {
  const first = s.split("\n")[0] ?? "";
  return first.length > max ? `${first.slice(0, max)}…` : first;
}

export interface ToolView {
  icon: ReactNode;
  title: ReactNode;
  /** A short fact beside the title, e.g. the lines changed. */
  meta?: string;
}

export function describe(name: string, raw: unknown): ToolView {
  const input = (raw && typeof raw === "object" ? raw : {}) as Input;
  const path = str(input.file_path);
  switch (name) {
    case "Bash":
      return { icon: <TerminalIcon />, title: <>Ran <Mono>{line(str(input.command))}</Mono></> };
    case "Read":
      return { icon: <FileTextIcon />, title: <>Read <Mono>{short(path)}</Mono></> };
    case "Write":
      return {
        icon: <FilePlusIcon />,
        title: <>Wrote <Mono>{short(path)}</Mono></>,
        meta: `${str(input.content).split("\n").length} lines`,
      };
    case "Edit": {
      const removed = str(input.old_string).split("\n").length;
      const added = str(input.new_string).split("\n").length;
      return { icon: <FilePenIcon />, title: <>Edited <Mono>{short(path)}</Mono></>, meta: `+${added} −${removed}` };
    }
    case "Grep":
      return { icon: <SearchIcon />, title: <>Searched for <Mono>{line(str(input.pattern), 48)}</Mono></> };
    case "Glob":
      return { icon: <FolderSearchIcon />, title: <>Found files <Mono>{str(input.pattern)}</Mono></> };
    case "list_sessions":
      return { icon: <ListIcon />, title: "Listed sessions" };
    case "create_session":
      return { icon: <SparklesIcon />, title: <>Created a session{input.git ? <> on <Mono>{str(input.git)}</Mono></> : null}</> };
    case "send_message":
      return { icon: <SendIcon />, title: <>Messaged <Mono>{str(input.session).slice(0, 8)}</Mono></> };
    case "interrupt_session":
      return { icon: <SquareIcon />, title: <>Interrupted <Mono>{str(input.session).slice(0, 8)}</Mono></> };
    case "start_session":
      return { icon: <PlayIcon />, title: <>Started <Mono>{str(input.session).slice(0, 8)}</Mono></> };
    default:
      return { icon: <WrenchIcon />, title: <Mono>{name}</Mono> };
  }
}

/** Read's output, `cat -n` style: its lines without their numbers, and the first number. */
function numbered(content: string): { text: string; start: number } | null {
  const lines = content.replace(/\n$/, "").split("\n");
  const m = /^\s*(\d+)\t/.exec(lines[0] ?? "");
  if (!m) return null;
  const body: string[] = [];
  for (const l of lines) {
    const n = /^\s*\d+\t(.*)$/.exec(l);
    if (!n) break; // the note after them ("[Showing lines …]")
    body.push(n[1] ?? "");
  }
  return { text: body.join("\n"), start: Number(m[1]) };
}

/** A tool call opened: its input and result. */
export function ToolDetail(props: {
  name: string;
  input: unknown;
  result?: { content: string; isError: boolean };
}) {
  const input = (props.input && typeof props.input === "object" ? props.input : {}) as Input;
  const result = props.result;
  const failed = result?.isError ? "destructive" : "default";
  const output = result && result.content.trim() && (
    <Code code={result.content} language="ansi" title={result.isError ? "Failed" : "Output"} variant={failed} wrap />
  );

  switch (props.name) {
    case "Bash":
      return (
        <div className="flex flex-col gap-2">
          <Code code={str(input.command)} language="bash" title={<><TerminalIcon /> Command</>} wrap />
          {output}
        </div>
      );
    case "Read": {
      const file = result && !result.isError ? numbered(result.content) : null;
      return file ? (
        <Code
          code={file.text}
          language={languageOf(str(input.file_path))}
          title={<><FileTextIcon /> {str(input.file_path)}</>}
          lineNumbers
          startLine={file.start}
        />
      ) : (
        output || null
      );
    }
    case "Write":
      return (
        <div className="flex flex-col gap-2">
          <Code
            code={str(input.content)}
            language={languageOf(str(input.file_path))}
            title={<><FilePlusIcon /> {str(input.file_path)}</>}
            lineNumbers
          />
          {result?.isError && output}
        </div>
      );
    case "Edit":
      return (
        <div className="flex flex-col gap-2">
          <Diff
            before={str(input.old_string)}
            after={str(input.new_string)}
            title={<><FilePenIcon /> {str(input.file_path)}</>}
          />
          {result?.isError && output}
        </div>
      );
    default:
      return (
        <div className="flex flex-col gap-2">
          {props.input !== undefined && <Code code={JSON.stringify(props.input, null, 2)} language="json" title="Input" wrap />}
          {output}
        </div>
      );
  }
}
