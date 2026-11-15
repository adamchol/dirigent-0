# Accounts and secrets

Who pays for your sessions' model calls, what secrets they get, and how they reach private GitHub
repositories. Everything here is per user: it applies to all your sessions, and no one else's.

## Anthropic API key

Sessions on the **Anthropic** provider call Claude with your API key. There is no shared key: until
you give yours, those sessions can't call a model.

- **Web**: account menu → **Anthropic API key**.
- **CLI**: `dirigent link anthropic` (reads the key from the terminal, hidden, or from stdin);
  `dirigent unlink anthropic` removes it.

control checks the key with Anthropic before keeping it, keeps it encrypted, and shows only its last
four characters after. Players get it on their lease, in memory; it's never in the log. Replacing
the key takes effect in running sessions at their next model call.

## ChatGPT plan

Sessions on the **ChatGPT** provider run on your ChatGPT plan, through OpenAI's API, as you'd use it
in Codex.

```sh
dirigent link chatgpt      # opens a browser to sign in to OpenAI; --no-browser prints the URL
dirigent models            # your plan's models
dirigent whoami            # shows the link, and whether it's broken
dirigent unlink chatgpt    # revokes it
```

Linking happens in the CLI (it needs a local redirect); the web's **ChatGPT plan** dialog shows the
commands and your link's state. control keeps the refresh token encrypted, renews access tokens
before they expire, and gives players short-lived ones. If OpenAI refuses the refresh token (you
signed out everywhere, changed password), the link is marked **broken**: link it again. When your
plan's usage limit is reached, turns fail saying so.

## Secrets

Secrets are values your sessions' commands need: tokens, API keys, passwords. They're environment
variables in the agent's Bash, and nowhere else.

- **Yours**, for all your sessions: account menu → **Secrets**, or `dirigent secret set NAME`.
- **A session's own**: session menu → **Secrets**, or `dirigent secret set NAME --session <id>`.
  A session's own secret wins over yours of the same name.

```sh
dirigent secret set NPM_TOKEN                 # prompts for the value, hidden (or reads stdin)
dirigent secret set DATABASE_URL --session 1f0c…
dirigent secret list [--session <id>]         # names only
dirigent secret rm NPM_TOKEN
```

- **Names** are environment variable names: `[A-Z_][A-Z0-9_]*`, up to 64 characters.
- **Values** are up to 64 KiB.
- **Changes** reach running sessions at once: from the agent's next command on.

How they're kept safe:

- control stores them encrypted and sends them to players on the lease; players hold them in memory
  only, and pass them to Bash's environment, not their own.
- Every secret value (6 characters or longer) is replaced by `[secret:NAME]` in tool output before
  the model or the log sees it, and in workspace files mirrored into the log (they're put back when
  you `dirigent sync`).
- The agent is told never to print secrets, and to say when one it needs is missing. A
  [dirigent](dirigents.md#secrets) can ask you for one.

Values are never shown again, in the web or the CLI; only names are.

## GitHub

Private repositories on GitHub need one of:

### Your own token

Add a `GITHUB_TOKEN` secret (yours, or a session's). The session clones with it, and git in the
agent's commands uses it for github.com (so it can push, if the token may). A fine-grained token
limited to the repositories you want is best. Your token wins over the App.

### The GitHub App

When the install has a GitHub App (an operator sets it up, see
[Installing](../operating/installing.md#github-app)), connect it in account menu → **GitHub**:

- **Connect GitHub** installs the App on your account or organization, for the repositories you
  pick.
- **Find installations** links installations made already (for instance, an organization's).
- **Disconnect** forgets them.

Each session then gets a token for **its own repository only**, renewed hourly, as its
`GITHUB_TOKEN`; it can read and write that repository (contents and pull requests) and nothing else.

The new-session form checks a github.com repository as you type and says how your sessions would
read it: public, with your token, through the App, or not at all (and what to do).
