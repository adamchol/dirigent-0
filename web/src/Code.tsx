// Code as the timeline shows it: shadcn's code block, highlighted by shiki (lib/highlighter.tsx)
// as it streams in, with a title and a copy button; and a diff of two strings.

import type { ReactNode } from "react";

import {
  CodeBlock,
  CodeBlockActions,
  CodeBlockContent,
  CodeBlockCopyButton,
  CodeBlockHeader,
  CodeBlockLine,
  CodeBlockTitle,
} from "@/components/ui/code-block";
import { useHighlightedCode } from "@/hooks/use-highlighted-code";

/** A language shiki has (lib/highlighter.tsx), from a file's name; plain text otherwise. */
export function languageOf(path: string): string {
  const ext = path.split(/[/.]/).pop()?.toLowerCase() ?? "";
  const byExt: Record<string, string> = {
    sh: "bash",
    bash: "bash",
    css: "css",
    diff: "diff",
    html: "html",
    js: "javascript",
    mjs: "javascript",
    cjs: "javascript",
    json: "json",
    jsx: "jsx",
    md: "markdown",
    py: "python",
    rs: "rust",
    go: "go",
    nix: "nix",
    toml: "toml",
    sql: "sql",
    tsx: "tsx",
    ts: "typescript",
    yaml: "yaml",
    yml: "yaml",
  };
  return byExt[ext] ?? "text";
}

export function Code(props: {
  code: string;
  language: string;
  title?: ReactNode;
  lineNumbers?: boolean;
  startLine?: number;
  wrap?: boolean;
  variant?: "default" | "destructive";
  className?: string;
}) {
  const lines = useHighlightedCode(props.code, props.language);
  return (
    <CodeBlock size="sm" variant={props.variant} className={props.className}>
      {props.title !== undefined && (
        <CodeBlockHeader>
          <CodeBlockTitle>{props.title}</CodeBlockTitle>
          <CodeBlockActions>
            <CodeBlockCopyButton value={props.code} />
          </CodeBlockActions>
        </CodeBlockHeader>
      )}
      {props.title === undefined && (
        <CodeBlockActions>
          <CodeBlockCopyButton value={props.code} />
        </CodeBlockActions>
      )}
      <CodeBlockContent
        lineNumbers={props.lineNumbers}
        startLine={props.startLine}
        wrap={props.wrap}
        className="max-h-96"
      >
        {lines.map((line, i) => (
          <CodeBlockLine key={i}>{line}</CodeBlockLine>
        ))}
      </CodeBlockContent>
    </CodeBlock>
  );
}

/** An edit: the lines taken out, then those put in. */
export function Diff(props: { before: string; after: string; title?: ReactNode }) {
  const removed = props.before.split("\n");
  const added = props.after.split("\n");
  return (
    <CodeBlock size="sm">
      {props.title !== undefined && (
        <CodeBlockHeader>
          <CodeBlockTitle>{props.title}</CodeBlockTitle>
        </CodeBlockHeader>
      )}
      <CodeBlockContent className="max-h-96">
        {removed.map((line, i) => (
          <CodeBlockLine key={`-${i}`} variant="removed">
            {line}
          </CodeBlockLine>
        ))}
        {added.map((line, i) => (
          <CodeBlockLine key={`+${i}`} variant="added">
            {line}
          </CodeBlockLine>
        ))}
      </CodeBlockContent>
    </CodeBlock>
  );
}
