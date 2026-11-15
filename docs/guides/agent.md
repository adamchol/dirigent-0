# The agent

What a coder session's player does: how it's prompted, the tools it has, how it handles long
conversations, and what happens when a model call fails. (For dirigents' tools, see
[Dirigents](dirigents.md).)

## The system prompt

A coder's system prompt is fixed for its player's life. It has:

1. **dirigent's instructions** ([`player/prompts/system.md`](../../player/prompts/system.md)): it is a
   worker agent that works autonomously with no one watching; makes the smallest change that does
   the job, in the project's style; never reverts changes it didn't make; runs nothing destructive;
   doesn't commit, push or publish unless asked; verifies its work with the project's real tooling;
   and reports by leading with the outcome, citing `path:line`.
2. **The environment**: where it runs. In the cluster's VMs: root on NixOS with network access, with
   git, gh, curl, jq, ripgrep, Python, gcc and make; anything else through `nix shell nixpkgs#…`, a
   project's flake through `nix develop`, Python packages in a venv
   ([`environment.md`](../../player/prompts/environment.md)). On a [host](hosts.md): the host's system,
   its user, and what the host's owner wrote about it.
3. **The project's instructions**: `AGENTS.md` and `CLAUDE.md` at the workspace's root, if present,
   as they were when the player started.
4. **The working directory**.

So to tell sessions on a repository how to build, test or behave, put it in the repository's
`AGENTS.md` (or `CLAUDE.md`).

## Tools

A coder has six tools, with the names, parameters and messages of Claude Code's, so models trained on
those use them well. Every path must be absolute and inside the workspace (symlinks included).

| Tool | Does | Limits |
|---|---|---|
| **Bash** | Runs a command with `bash -c` in the workspace. | Default timeout 2 minutes, at most 10. Output (stdout and stderr together) over 30,000 characters keeps its first 20,000 and last 10,000. A nonzero exit is reported as an error with its code. |
| **Read** | Reads a file, numbered like `cat -n`. | 2,000 lines by default (`offset`, `limit` for more); lines over 2,000 characters are cut; at most 50 KB per call; files over 4 MiB and binary files are refused. |
| **Write** | Writes a whole file, making parent directories. | An existing file must have been read first, and not changed since. |
| **Edit** | Replaces an exact string in a file (or every occurrence, with `replace_all`). | The file must have been read and not changed since; the string must occur exactly once otherwise. |
| **Glob** | Finds files by pattern, newest first. | Respects `.gitignore`, skips hidden files; at most 100 results. |
| **Grep** | Searches file contents (ripgrep's engine): files with matches, matching lines with context, or counts. | Respects `.gitignore`; 250 results by default (`head_limit`); lines over 500 characters cut. |

Read, Glob and Grep calls in one reply run at the same time; Bash, Write and Edit run one after
another, in the order the model called them.

### What commands see

Bash runs with a clean environment: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `TERM`,
`TMPDIR`, `TZ` and `LC_*`, plus **the session's secrets** as variables. With a `GITHUB_TOKEN`
secret, git also authenticates to github.com with it. Commands run in their own process group: a
timeout or an interrupt kills all of it.

Secret values are redacted from what the tools return (each becomes `[secret:NAME]`), so they never
reach the model or the log. See [Accounts and secrets](accounts-and-secrets.md#secrets).

## Turns, steering and interrupts

A turn runs model calls and tool calls until the model answers without tools. While it runs:

- **A message you send** is added to the conversation at the model's next step: after the running
  tools' results if it called tools, or as a new call if it was just writing.
- **An interrupt** cancels the model call and kills running commands. Tool calls that didn't finish
  get "aborted" results; the model is told at its next turn that the previous one was interrupted
  and that tools may have partly run.

## Compaction

Long sessions are compacted before they overflow the model's context window:

- **When**: before a model call whose context would be 85% of the window or more, or right after the
  provider says the prompt is too long. At most once between two replies.
- **What's kept**: the most recent part of the conversation, about a tenth of the window (at most
  20,000 tokens), verbatim; never a tool call without its result.
- **What replaces the rest**: a summary the same model writes, with fixed sections: *Goal* (quoting
  your latest request), *Constraints and preferences*, *Progress* (done, in progress, blocked), *Key
  decisions*, *Next steps*, *Critical context*; followed by lists of the files it read and modified.
- **If it fails** (the summary is empty, cut off, missing its sections, or no shorter), the call goes
  on without it; a second overflow fails the turn.

A compaction is an event in the log (`compacted`), so a new player gets the same compacted
conversation. The agent also renames the session from the summary, unless you keep its name.

## Titles

After the first message, a small model (`claude-haiku-5-5` on Anthropic; the session's own model on
ChatGPT) names the session: a title of up to six words and a one-sentence description, in your
language. It renames it after each compaction. Your own rename, kept, stops this.

## Errors and retries

| Error | What happens |
|---|---|
| Rate limits (429), overloaded or 5xx responses, dropped connections, empty replies | Retried, waiting what the provider asks (`Retry-After`), or 1 s doubling up to 60 s; without limit, until it works or you interrupt. Each wait is shown in the chat. |
| The prompt is too long | Compacted, then retried once. |
| Other 4xx (a refused key, a bad request) | The turn fails with the provider's message (`API error 400: …`). The session waits for your next message. |
| ChatGPT plan's usage limit | The turn fails, saying the plan's limit is reached. |
| ChatGPT access token refused | control renews it, and the call is retried once. |

A failed turn doesn't stop the session: send another message when the cause is fixed.

## Checkpoints and the mirror

After every turn the player checkpoints the workspace, and after every Bash, Write or Edit it mirrors
the workspace's files into the log. Both are covered in [Workspaces](workspaces.md).

## Prompt caching

On Anthropic, the system prompt and tools form a stable prefix, cached across calls, and the
conversation is cached up to its latest messages; compaction reuses the same cache. On ChatGPT,
the session's id is the prompt cache key. Usage in the chat shows cache reads separately.
