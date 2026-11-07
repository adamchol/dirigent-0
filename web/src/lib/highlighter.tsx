import * as React from "react"
import {
  createHighlighterCore,
  type GrammarState,
  type HighlighterCore,
  type ThemeRegistration,
} from "shiki/core"
import { createJavaScriptRegexEngine } from "shiki/engine/javascript"

// Add the languages you need. Each one loads the first time it is used.
const languages = {
  bash: () => import("shiki/langs/shellscript.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  nix: () => import("shiki/langs/nix.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  typescript: () => import("shiki/langs/typescript.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
}

const aliases: Record<string, keyof typeof languages> = {
  js: "javascript",
  md: "markdown",
  py: "python",
  rs: "rust",
  sh: "bash",
  shell: "bash",
  shellscript: "bash",
  ts: "typescript",
  yml: "yaml",
  zsh: "bash",
}

// dirigent's colours for code: its one accent for keywords, green for literals, violet for types
// and attributes, the rest in its text greys (index.css)
function theme(name: string, type: "light" | "dark", c: Record<string, string>): ThemeRegistration {
  return {
    name,
    type,
    colors: { "editor.foreground": c.text!, "editor.background": "#00000000" },
    tokenColors: [
      { settings: { foreground: c.text } },
      { scope: ["comment", "punctuation.definition.comment"], settings: { foreground: c.comment, fontStyle: "italic" } },
      { scope: ["keyword", "storage", "keyword.operator.new", "keyword.control"], settings: { foreground: c.keyword } },
      { scope: ["string", "constant.numeric", "constant.language", "constant.character"], settings: { foreground: c.literal } },
      { scope: ["entity.name.type", "support.type", "entity.name.class", "meta.attribute", "entity.other.attribute-name", "support.class"], settings: { foreground: c.type } },
      { scope: ["entity.name.function", "support.function", "meta.function-call"], settings: { foreground: c.function } },
      { scope: ["punctuation", "keyword.operator", "meta.brace"], settings: { foreground: c.punctuation } },
      { scope: ["markup.inserted"], settings: { foreground: c.literal } },
      { scope: ["markup.deleted"], settings: { foreground: c.deleted } },
    ],
  }
}

const themes = [
  theme("dirigent-dark", "dark", {
    text: "#c9ccd1",
    comment: "#6a6f78",
    keyword: "#e8a33d",
    literal: "#92d2a9",
    type: "#c3a3e8",
    function: "#edeef0",
    punctuation: "#8b9098",
    deleted: "#e39a88",
  }),
  theme("dirigent-light", "light", {
    text: "#27272a",
    comment: "#8b8b95",
    keyword: "#a6640d",
    literal: "#1f7a4d",
    type: "#7c3aed",
    function: "#18181b",
    punctuation: "#71717a",
    deleted: "#b4401e",
  }),
]

let highlighter: Promise<HighlighterCore> | undefined

function getHighlighter() {
  highlighter ??= createHighlighterCore({
    themes,
    langs: [],
    engine: createJavaScriptRegexEngine({ forgiving: true }),
  })

  return highlighter
}

function getLanguage(language: string) {
  const name = language.toLowerCase()

  if (name === "ansi") {
    return name
  }

  if (name in languages) {
    return name as keyof typeof languages
  }

  return aliases[name] ?? null
}

// Loads a language and returns a highlighter for it, or null for a language
// that is not listed.
export async function loadHighlighter(language: string) {
  const lang = getLanguage(language)

  if (!lang) {
    return null
  }

  const shiki = await getHighlighter()

  if (lang !== "ansi" && !shiki.getLoadedLanguages().includes(lang)) {
    await shiki.loadLanguage(languages[lang])
  }

  return {
    // Returns one React node per line, and the grammar state after the last
    // line. Pass that state back to continue where the code left off.
    highlight(code: string, state?: GrammarState) {
      const tokenize = (text: string) =>
        shiki.codeToTokens(text, {
          lang,
          themes: { light: "dirigent-light", dark: "dirigent-dark" },
          defaultColor: false,
          grammarState: state,
        }).tokens

      // ANSI has no grammar state, so each line is tokenized on its own and
      // gets the same colors whether it arrives in one piece or streams.
      const tokens =
        lang === "ansi"
          ? code.split("\n").flatMap((line) => tokenize(line))
          : tokenize(code)

      return {
        lines: tokens.map((line) =>
          line.map((token, index) => (
            <span
              key={index}
              style={token.htmlStyle as React.CSSProperties}
              className="text-(--shiki-light) dark:text-(--shiki-dark)"
            >
              {token.content}
            </span>
          ))
        ),
        state: shiki.getLastGrammarState(tokens),
      }
    },
  }
}

// Returns one React node per line, or null for a language that is not listed.
export async function highlightCode(code: string, language: string) {
  const highlighter = await loadHighlighter(language)

  return highlighter ? highlighter.highlight(code).lines : null
}
