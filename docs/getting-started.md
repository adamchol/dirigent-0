# Getting started

This walks you through dirigent twice: first on your own machine, with the development stack and a
mock model (no API key, no Kubernetes, nothing paid), then on a real install.

## Part 1: on your machine, with a mock model

### What you need

- Linux (x86_64 or aarch64) with [Nix](https://nixos.org/download) and flakes enabled.
- About 8 GB of memory and 20 GB of disk: the stack runs Redpanda, ClickHouse, Kafka Connect,
  Postgres, Garage (S3), chlive, control and the web, and builds control and the player.

### Start the stack

```sh
git clone https://github.com/adamchol/dirigent-0
cd dirigent-0
nix develop        # the dev shell: Rust, Bun, and the stack's tools
dnvr up            # starts every process, in order
```

`dnvr ps` shows the processes, `dnvr logs control` one of their logs, and `dnvr down` stops
everything. The first start takes a while: it builds control and the player.

The stack runs control in *declared* auth: there is no sign-in, and you are whoever you say you
are, `user:dev` by default. Its players run as local processes (not VMs), on the mock model.

### Make a session

Open <http://127.0.0.1:54340>.

1. Click **New session**. Leave **Coder** selected and the **Repository** empty (an empty
   workspace), or give a public git URL to clone.
2. Click **Make it**. A player starts: the status bar at the bottom says **Starting**, then
   **Workspace**, and the session's dot in the list turns solid when the player is attached.
3. Type a message and press Enter. The mock model understands a few inputs:
   - plain text: it echoes your text, then streams a few paragraphs of lorem ipsum;
   - `!ls -la`: it calls the Bash tool with `ls -la`, then replies;
   - `!!<Tool> <json>`: it calls any tool with that input, e.g. `!!Glob {"pattern": "**/*"}`;
   - `?demo`: it "thinks", then replies with Markdown and a code block.
4. While a reply streams, send another message: it *steers* the running turn. The **Stop** button
   in the composer interrupts the turn.

The session's menu (**…** at the top) has **Stop**, **Rename**, **Secrets**, **Open locally**,
**Archive** and more; see [The web UI](guides/web.md).

### Follow the workspace locally

In another terminal, in the dev shell:

```sh
dnvr dirigent login http://127.0.0.1:54340     # declared auth: you're user:dev
dnvr dirigent sync <session-id>                # the id is in the session's menu
```

`sync` writes the session's files into `./<session title>` and keeps them in step as the agent
changes them, until you press Ctrl-C. Send `!echo hello > hello.txt` in the web and watch the file
appear. See [Workspaces](guides/workspaces.md).

### Use a real model

Real models cost money. To try one, start the stack without the mock:

```sh
dnvr down
DIRIGENT_PLAYER_ARGS= dnvr up
```

Then add your Anthropic API key in the web's account menu (bottom left, **Anthropic API key**), or
link a ChatGPT plan with `dnvr dirigent link chatgpt`. New sessions run on it.

## Part 2: on a real install

Someone (maybe you) has installed dirigent on a cluster ([Installing on
Kubernetes](operating/installing.md)) and given you its address, say `https://dirigent.example.com`.

1. **Sign in.** Open the address and sign in with Google.
2. **Say who pays.** In the account menu, add your **Anthropic API key**; or install the CLI and
   link your ChatGPT plan with `dirigent link chatgpt`. Sessions can't call a model until you do.
   See [Accounts and secrets](guides/accounts-and-secrets.md).
3. **Give access to private repositories**, if you'll use any: connect GitHub in the account menu
   (when the install has a GitHub App), or add a `GITHUB_TOKEN` secret.
4. **Make a session**: **New session**, a repository URL (the form checks that your sessions can
   read it), a model, how much it thinks, and a first message. Each session gets a VM of its own;
   the first start takes a minute or so while it boots.
5. **Install the CLI**, to follow workspaces locally, import sessions, or run sessions on your own
   machines:

   ```sh
   brew install adamchol/dirigent/dirigent          # macOS or Linux with Homebrew
   # or: nix profile install github:adamchol/dirigent-0#cli
   dirigent login https://dirigent.example.com
   dirigent sync <session-id>
   ```

## Where next

- [Concepts](concepts.md), to know what a session, a player and the log are.
- [Sessions](guides/sessions.md) and [The agent](guides/agent.md), for working with sessions.
- [Dirigents](guides/dirigents.md), to have an agent direct your sessions.
- [Hosts](guides/hosts.md), to run sessions on your own machines.
- [Importing](guides/importing.md), to continue a Claude Code or Codex session here.
