# Concepts

The handful of ideas the rest of the docs build on.

## Sessions

A **session** is one agent's work: a conversation and a workspace. It has an id (a UUID) and a
**spec**, set when it's made and fixed after:

| Spec | What | Default |
|---|---|---|
| model | The model, e.g. `claude-sonnet-5`, or one of your ChatGPT plan's | `claude-sonnet-5` |
| effort | How much the model thinks: none, `low`, `medium`, `high`, `max` | `medium` |
| provider | Who the model calls are paid to: `anthropic` (your API key) or `chatgpt` (your plan) | `anthropic` |
| source | A git repository (and ref) cloned into the workspace, or none (an empty workspace) | none |
| part | `coder` or `dirigent` (below) | `coder` |
| machine | Cores, memory and workspace disk of its VM | the default machine when it starts |

Besides its spec, a session has an **owner** (who made it), **viewers**, **secrets**, a **title**
and description (the agent names it; you can rename it), and a **placement**: the cluster, or one of
your hosts.

### Coders and dirigents

- A **coder** works in a workspace with Bash and file tools. It's what you use for code.
- A **dirigent** has no workspace. Its tools make, message, read, start and stop *your other
  sessions*; it follows them as they finish and asks you for secrets they need. You talk to it and
  it directs coders. See [Dirigents](guides/dirigents.md).

### Owners and viewers

Users are **subjects**: `user:<email>` when signed in with Google. A session's owner is the only one
who writes to it (messages, interrupts, renames), starts and stops it, manages its viewers and
secrets, archives and deletes it. **Viewers** read it, live, and can follow its workspace with
`dirigent sync`. A dirigent acts as its owner: it reaches exactly the sessions you own or view.

## Players

A **player** is the agent process that runs a session. One player at a time holds a session's
**lease**, given by control. Each new holder starts a new **epoch** of the session (1, 2, …).

A session is in one of three states, shown as its dot in the web:

| State | Means |
|---|---|
| **idle** | No player. Messages wait in the log; sending one starts a player. |
| **active** | A player holds the lease and is connected. |
| **away** | A player holds the lease but isn't connected right now (starting, or briefly cut off). |

Players are replaceable: when one stops (you stopped the session, its VM was deleted, it failed),
the next one picks up from the log exactly where it was. A session isn't tied to a machine.

## The log

Everything a session does is an **event** in its log: your messages, the model's replies and
streamed deltas, tool calls and results, turn starts and ends, compactions, checkpoints, workspace
changes, players attaching and leaving. The log is append-only; a player rebuilds the conversation
by reading it.

You don't call an API to talk to a session: you **write an input into its log** (a message, an
interrupt), and its player reacts. The web, the CLI and dirigents all do this, and anyone who can
read the session sees it live. See [Events](reference/events.md) for the full list.

## Turns

A **turn** starts from an input (your message). The player calls the model; the model replies, often
with tool calls; the player runs them and calls the model again, until the model replies without
tools. Then the turn ends:

- **completed**: the model was done;
- **aborted**: you interrupted it, or stopped the session "now";
- **failed**: the model call failed for good (e.g. a refused key).

A message sent while a turn runs **steers** it: the model sees it at its next call. Messages sent
while the session is idle queue up; each queued message gets a turn of its own.

## Workspaces

A coder's **workspace** is a directory: a clone of its source, or empty. Two things keep it:

- **Checkpoints**: after every turn, the workspace is snapshotted to S3 (kopia). A player that finds
  it gone (a new VM) restores the last one.
- **The mirror**: after every change the agent makes, the workspace's files are committed into the
  log as git bundles. `dirigent sync` checks them out on your machine; a session that moves
  between machines gets its files from them.

See [Workspaces](guides/workspaces.md).

## Where sessions run

- **The cluster** (default): each player runs in a KubeVirt VM of its own, running NixOS, sized by
  the session's machine. Warm VMs can be booted ahead so starts skip the boot. The cluster can have
  its own NixOS configuration for these VMs. See [Machines and
  systems](operating/machines-and-systems.md).
- **Your hosts**: a machine of yours running `dirigent host` runs your sessions placed on it, as
  local processes, in directories you choose. See [Hosts](guides/hosts.md).

A session's placement takes effect at its next start; it carries its conversation and its files
with it.

## Who pays

Model calls are made by the player, with its owner's credentials, sent to it by control on the
lease: the owner's **Anthropic API key**, or an access token for the owner's **ChatGPT plan**. Keys
are kept encrypted by control and never written to the log. See [Accounts and
secrets](guides/accounts-and-secrets.md).

## A session's life

```
 made ──▶ idle ◀──────────── stopped (turn finished, or cut "now")
           │  ▲
  message  │  │  player detaches / fails
  or start ▼  │
          active ──── turns ────▶ waits for the next message
           │
 archived ─┴─▶ (no player takes it) ──▶ deleted (its log, checkpoints and secrets, for good)
```

- **Stopping** a session lets the running turn finish (or cancels it, with "stop now"), then its
  player leaves; the session is idle.
- **Archiving** an idle session keeps everything but lets no player take it; **unarchiving**
  undoes it.
- **Deleting** an archived session removes its events, checkpoints and secrets for good.
