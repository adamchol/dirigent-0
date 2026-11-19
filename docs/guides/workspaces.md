# Workspaces

A coder's workspace is a directory: a clone of its source, or empty. It outlives any one machine:
it's checkpointed after every turn and mirrored into the log after every change. This page covers
both, and how to follow a workspace on your own machine.

## Where it comes from

When a session's first player starts, it clones the source (a partial clone of the ref, or of the
default branch), or makes an empty directory. The chat shows **Cloning owner/repo…** meanwhile. If
GitHub refuses, the error says how to give access ([GitHub](accounts-and-secrets.md#github)).

## Checkpoints

After every turn, the player snapshots the workspace to S3 with [kopia](https://kopia.io) (the
`checkpoint` event). Snapshots are deduplicated, keep files of any size, leave out what
`.gitignore` excludes, and never expire while the session exists. The next turn waits for the
checkpoint.

A player that starts on a fresh machine (a new VM) restores the last checkpoint (**Restoring the
workspace from its checkpoint…**), then brings it up to date from the mirror.

Checkpoints are made in the cluster. Sessions on [hosts](hosts.md) don't checkpoint: their
workspace stays in the host's directory, and the mirror covers moves.

## The mirror in the log

After every Bash, Write or Edit call (and when a player starts), the player commits the
workspace's files into a git repository of its own, kept apart from the workspace's `.git`, and
writes the new objects into the log as a git bundle (`files_changed`). It takes:

- every file `.gitignore` doesn't exclude, hidden ones included;
- not `.git` directories, and not files over 50 MB.

Included files are saved and restored verbatim. A credential written to one of these files
will be in the session's workspace history. Literal `[secret:NAME]` markers, including those
in older snapshots, are restored as text, never expanded.

The workspace's own git history isn't touched: the agent's commits, branches and stashes stay the
agent's business.

The mirror is what lets you follow a workspace live, and what a session moving between machines
takes along.

## Following a workspace: `dirigent sync`

```sh
dirigent login https://dirigent.example.com
dirigent sync <session-id> [dir]
```

- Writes the session's files into `dir` (by default `./<session title>`) and keeps them in step as
  the agent works, until Ctrl-C. `--once` syncs once and exits.
- Owners and viewers can sync.
- Only files the workspace has are written; ignored files and files over 50 MB aren't in the log, so
  they aren't synced.
- If you changed a file locally and the session changes it too, yours is moved aside to
  `<file>.local`.
- It keeps its own git repository in `~/.cache/dirigent/sync/`, one per session and folder. It needs
  `git`.
- Before the session's first mirror, it says it has nothing yet and waits.

It's read-only towards the session: to give the agent files, commit them to the source, or ask the
agent to fetch them.

## When the workspace is gone

A player starting where the workspace isn't (a new VM, a different host) makes it again:

| It has | It does |
|---|---|
| A checkpoint | Restores it, then checks out the log's latest files over it (changes after the checkpoint come back too). |
| No checkpoint, but files in the log | Clones the source (or makes it empty), then checks out the log's latest files: **workspace made again, with the session's files**. |
| Neither | Clones the source again. |

A workspace a [host](hosts.md) kept while the session ran elsewhere is brought up to the log's latest
files in the same way: what changed or was deleted elsewhere is changed or deleted here; ignored
files stay as they were.

What the mirror doesn't keep (ignored files, files over 50 MB) comes back only from a checkpoint.
The agent is told when its workspace was made again, and what may be missing (for example, build
outputs and installed dependencies, which are usually ignored).
