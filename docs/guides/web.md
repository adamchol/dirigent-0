# The web UI

The web is where you make sessions, talk to them, and watch them work. It reads every session's log
live, so what you see is what the player is doing right now.

## Signing in

- **Google** (`oidc` auth, the default for installs): sign in with your Google account. Your
  sign-in lasts 30 days unused, in an HttpOnly cookie, and renews itself while you use it. **Sign
  out** is in the account menu.
- **Declared** (development installs): there's no sign-in. You are `user:dev`; the account menu's
  **Look as** field switches to any other subject, which is handy for trying viewers.

## Layout

- **The sidebar** (a sheet on phones) lists your sessions, with **New session** at the bottom and
  the account menu below it.
- **The top bar** shows the open session's title and description, its **Viewers** (yours only), a
  **Start** button when it's idle, and its **…** menu.
- **The chat** is the session's log, with the composer at the bottom.
- **The status bar** shows the connection (**Live**, **Connecting**, **Offline**), the player
  (`Player <id> · epoch N · <its version>`, with `· away` while it's disconnected, or **Idle**), what it's doing
  (**Working**, **Starting**, **Workspace**, **Starting turn**), and on the right the model, effort,
  machine, and how much context the conversation uses.

The open session is in the URL (`#<session-id>`), so a link opens it.

## The sessions list

Sessions are ordered by their latest activity (renames don't count). The filter matches titles and
repositories. Sessions others shared with you are under **Shared with you**; archived ones under
**Archived**.

Each session's dot is its state: filled and haloed when **active** (a player is attached), grey when
**away**, a ring when **idle**. Its name is its title, or its repository (`owner/repo@ref`),
**Empty workspace**, or **Dirigent** until it has one.

Hovering a session shows its **More** menu: Start, Stop, Stop now, Rename, Secrets, Open locally,
Copy ID, Archive (or Unarchive, and Delete… once archived).

## Making a session

**New session** opens the form:

| Field | What |
|---|---|
| **Coder / Dirigent** | A coder works in a repository; a dirigent directs your sessions and has no repository. |
| **Repository** | A git URL, or empty for an empty workspace. For a github.com URL the form checks, as you type, whether your sessions can read it: **Public**, **Readable with your GITHUB_TOKEN**, **Readable through the GitHub App**, or why not, with a link to fix it. |
| **Ref** | A branch, tag or commit; empty for the default branch. |
| **Paid with** | **Anthropic API** or **ChatGPT plan** (only once you've linked a working plan). |
| **Model** | Free text for Anthropic (default `claude-sonnet-5`); a list of your plan's models for ChatGPT. |
| **Thinking** | No thinking, Low, Medium (default), High, Max. |
| **Runs on** | Shown when you have hosts connected: **The cluster**, or one of your hosts. |
| **Machine** | Cores, memory and disk, for the cluster. Left at the default, the session gets the default machine as it is when it starts. A machine other than the default can't use a warm VM, so it starts slower. |
| **Start it** | On (default): a player takes it at once. |

**Make it** creates the session. It's yours; nothing about it can be changed later except its
title, viewers, secrets and where it runs.

## The chat

### Writing

Type in the composer and press **Enter** (Shift+Enter for a new line). Your message shows as
**Sending**, then **Queued** until a turn takes it. If the session is idle, sending starts it.

While a turn runs, the composer says **Steer the running turn…**: what you send joins that turn,
and the model reads it at its next step. The **Stop** button next to it interrupts the turn; the
session stays up and waits for your next message.

Only the owner can write. Viewers see why the composer is closed, and so does anyone looking at an
archived session.

### Reading

The chat shows your messages, the replies as they stream, thinking (collapsed: **Thinking**, then
**Thought**), and tool calls as one line each (**Ran**, **Read**, **Wrote**, **Edited**,
**Searched for**, **Found files**…), expandable to their input and output. Each turn ends with a
marker (**Done**, **Interrupted**, **Aborted**, **Failed**) and its token usage.

While a session starts, a line says what it's waiting on: **Starting a machine…**, **Restoring the
workspace from its checkpoint…**, **Cloning owner/repo…**, **Preparing the workspace…**, **Starting
the turn…**.

How much of the log shows is a setting (**Settings → The chat shows**):

- **Conversation**: messages, replies, tools, turns and errors.
- **Activity** (default): also players attaching and leaving, the workspace, checkpoints, titles.
- **All events**: every event, with its data.

### Banners

- **Viewing someone's session**: you're a viewer; only the owner writes to it.
- **No ChatGPT plan is linked** / **The ChatGPT link is broken**: the session runs on a plan that
  can't be used; **Reconnect** shows how to link it again.
- **A secret request** (from a dirigent): **Session … needs NAME**, with **Provide** and **Decline**.
  See [Dirigents](dirigents.md#secrets).

## The session menu

The **…** at the top:

| Item | What |
|---|---|
| **Rename** | Title (up to 80 characters) and description (up to 300). **Keep this name** (on by default) stops the agent from renaming it; off, it names it again as the work moves on. |
| **Secrets** | The session's own secrets, and the names it gets from yours. See [Accounts and secrets](accounts-and-secrets.md#secrets). |
| **Model** | Its model, effort and provider; running, it takes them from its next call. See [Sessions](sessions.md#switching-model-provider-or-thinking). |
| **Runs on** | (Idle sessions.) The cluster or one of your hosts, for its next start. A session placed on a host that isn't connected shows it as **(not connected)**. |
| **Open locally** | The commands to follow its workspace on your machine (`dirigent login`, `dirigent sync`). |
| **Copy ID** | The session's id. |
| **Stop** / **Stop now** | (Running sessions.) Stop lets the turn finish first; Stop now cancels it. |
| **Archive** / **Unarchive** | (Idle sessions.) An archived session can't be written to or started. |
| **Delete…** | (Archived sessions.) Deletes its conversation, checkpoints and secrets for good. |

**Viewers** (top bar) lists who else can read the session; add a subject such as
`user:someone@example.com`, or remove one.

## The account menu

At the bottom of the sidebar:

- **ChatGPT plan**: how to link it (from the CLI), and whether it's linked or broken.
- **Anthropic API key**: add, replace or remove your key. It's checked with Anthropic, kept
  encrypted, and only its last four characters are shown again.
- **Secrets**: your secrets for all your sessions.
- **GitHub**: connect the install's GitHub App, or add your own `GITHUB_TOKEN`.
- **Settings**:
  - **The chat shows**: Conversation, Activity or All events.
  - **New sessions' machines**: the default machine, or your own default when the install lets
    users set one; and which NixOS system new sessions get.
  - **Your hosts**: your machines connected now with `dirigent host`.
- **About and debug info**: what runs, for bug reports: the web's and control's versions (control's
  start and runner), the players' VM image and system, Postgres's and ClickHouse's versions, in
  Kubernetes the install's pods (component, images, ready, restarts), your hosts' versions, and the
  open session's player (its version and where it runs). **Copy debug info** copies it all as JSON.
- **Theme**: Dark, Light or System.
- **Sign out** (Google sign-in only).

Settings are kept by control, so they follow you to every browser.
