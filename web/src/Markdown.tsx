// The model's text, as Markdown (GitHub's flavour): fenced code in code blocks, the rest styled
// to sit in a message.

import { memo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

import { Code } from "./Code.tsx";

const components: Components = {
  pre: ({ children }) => <>{children}</>,
  code({ className, children }) {
    const text = String(children ?? "");
    const fence = /language-(\S+)/.exec(className ?? "");
    // fenced (it has a language, or spans lines): a block; else inline
    if (fence || text.includes("\n")) return <Code code={text} language={fence?.[1] ?? "text"} title={fence?.[1]} />;
    return <code className="rounded-md bg-muted px-1.5 py-0.5 font-mono text-[0.85em]">{children}</code>;
  },
  a: ({ children, href }) => (
    <a href={href} target="_blank" rel="noreferrer" className="font-medium underline underline-offset-3 hover:text-foreground/80">
      {children}
    </a>
  ),
  p: ({ children }) => <p className="leading-relaxed">{children}</p>,
  ul: ({ children }) => <ul className="ml-5 list-disc space-y-1 marker:text-muted-foreground">{children}</ul>,
  ol: ({ children }) => <ol className="ml-5 list-decimal space-y-1 marker:text-muted-foreground">{children}</ol>,
  h1: ({ children }) => <h3 className="mt-2 text-base font-semibold">{children}</h3>,
  h2: ({ children }) => <h3 className="mt-2 text-base font-semibold">{children}</h3>,
  h3: ({ children }) => <h4 className="mt-1 font-semibold">{children}</h4>,
  h4: ({ children }) => <h4 className="mt-1 font-medium">{children}</h4>,
  blockquote: ({ children }) => <blockquote className="border-l-2 pl-3 text-muted-foreground">{children}</blockquote>,
  hr: () => <hr className="border-border" />,
  table: ({ children }) => (
    <div className="overflow-x-auto rounded-lg border">
      <table className="w-full text-sm">{children}</table>
    </div>
  ),
  th: ({ children }) => <th className="border-b bg-muted/50 px-3 py-1.5 text-left font-medium">{children}</th>,
  td: ({ children }) => <td className="border-b px-3 py-1.5 last:border-b-0">{children}</td>,
};

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="flex min-w-0 flex-col gap-3 text-sm">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
