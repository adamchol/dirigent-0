# Importing

`dirigent import` turns a Claude Code or Codex session from your machine into a dirigent session
that goes on from where it was: its conversation, and its project's files as they are now.

```sh
cd ~/code/app
dirigent import claude-code --dry-run   # what would be imported
dirigent import claude-code             # this directory's latest Claude Code session
dirigent import codex 0199a3f2-…        # a Codex session, by id
```

The new session isn't started: open it in the web and send a message; its player takes the
conversation and goes on.

## Which session

| Agent | Where it's read from | Without an id |
|---|---|---|
| `claude-code` | `~/.claude/projects/<this directory>/<id>.jsonl` (`$CLAUDE_CONFIG_DIR` instead of `~/.claude`) | The most recent session of this directory |
| `codex` | `~/.codex/sessions/YYYY/MM/DD/rollout-…-<id>.jsonl` (`$CODEX_HOME` instead of `~/.codex`) | The most recent session started in this directory |

An id is looked up across all your projects; a path to a `.jsonl` file is read as it is.

The Codex reader hasn't been tried on a real Codex session yet: use `--dry-run` first.

## What's imported

**The conversation**: your messages, the replies, tool calls and their results, compactions, and the
title (a title you set in Claude Code is kept as a held name). Thinking is kept to read, but never
sent to a model again. Tools dirigent doesn't have stay in the history as they were called.
Subagents' transcripts aren't imported. Very long texts (over 200,000 characters) are clipped in the
middle.

**The workspace**: the project's git remote (`origin`) and current branch become the session's
source, and the project's files *as they are on your machine*, committed or not, untracked included,
ignored ones not, go on top as the session's first files. A project that isn't a git repository has
an empty source, and its files are imported only if you name it with `--dir`.

**The spec**: the model the session last used (`--model` to change it), effort `medium` (`--effort`),
and Anthropic for Claude Code sessions, your ChatGPT plan for Codex ones (`--provider`).

The web marks where the imported conversation ends, with the dates it really had; the session's
events themselves carry the time of the import.

## Redaction

In imported tool output, text that looks like a key or token is replaced with `[redacted]` before
upload. Workspace files, including changed and untracked files, are imported as-is. The tool
output check looks for Anthropic, OpenAI, GitHub, GitLab, AWS, Slack, Google, Stripe keys
and PEM private keys. `--dry-run` says how many it found; `--no-redact` turns it off.

## Options

| Option | |
|---|---|
| `[session]` | An id, or a path to a `.jsonl` transcript. Default: this directory's most recent. |
| `--dir <path>` | The project, instead of the session's working directory. |
| `--model <m>`, `--effort <e>`, `--provider anthropic\|chatgpt` | The new session's spec. |
| `--dry-run` | Print what would be imported: events, turns, size, dates, title, spec, workspace, redactions. |
| `--again` | Import into a new session, even if this one was imported before. |
| `--no-redact` | Keep keys and tokens as they are. |
| `--chlive <url>` | chlive's address, if not the default from your sign-in. |

Importing the same session twice says where it went the first time (the session's id is derived
from you, the agent, and its session id). If an import fails halfway, its session is deleted; try
again.

## Afterwards

The agent is told the conversation was imported and where it worked on your machine, so it knows
paths in the history refer to your machine's directory, not its workspace. Your project stays as it
was; follow the session's changes with [`dirigent sync`](workspaces.md#following-a-workspace-dirigent-sync).
