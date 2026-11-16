# Dirigents

A **dirigent** is a session that directs your other sessions. You tell it what you want done; it
splits the work into tasks, makes a coder session for each (or reuses one), sends them messages,
waits for them to finish, checks what they did, and reports back. You can keep talking to the
coders yourself at the same time.

## Making one

In the web: **New session → Dirigent**. It has no repository; pick its model, thinking and who
pays as for any session. A dirigent can't be given a source.

## What it can do

A dirigent acts **as you**: it reaches exactly the sessions you own or view, with your rights. Its
messages are marked as coming from it (`Dirigent <id>` in the chat).

| Tool | Does |
|---|---|
| `list_sessions` | Lists your sessions (optionally those on a repository): id, part, state, model, machine, source, your role, and the names of their secrets. |
| `read_session` | Reads a session's recent turns in brief: the inputs, the tools used (and how many failed), the last reply, how each turn ended, and queued messages. Works on sessions you view. |
| `create_session` | Makes and starts a coder session with a first message: a repository and ref, a model, effort, and a machine (cores, memory, disk). |
| `send_message` | Messages a session you own, starting it if idle; a running session takes it as steering. |
| `interrupt_session` | Interrupts a session's running turn. |
| `start_session` / `stop_session` | Starts a session, or stops it (`now` to cancel the turn). |
| `update_session` | Changes a session you own: its model, effort or provider (from its next model call), and its machine (an idle one starts on it next; a running one moves once its turn is done; the disk only grows). Reports each part, one refused not undoing the other. |
| `request_secret` | Asks you for a secret a session you own needs (below). |

It has no Bash and no files: it learns what coders did by reading their sessions, and changes code
only through them.

What it can't do: make dirigents, make sessions paid with your ChatGPT plan (they're on Anthropic),
place sessions on your hosts, or add viewers.

## Following sessions

A dirigent doesn't poll. When it makes or messages a session, it follows that session from then on:
each time the session's turn ends, its player fails, or it's stopped, an update arrives in the
dirigent's log (`session_update`, with the session's last reply). An update is an input: it starts
the dirigent's next turn, or steers the running one. So you can give a dirigent a large job and
leave; it carries on as its coders finish.

A new player for the dirigent picks the following up from its log.

## Secrets

When a coder needs a secret it doesn't have (a token to push, an API key to test with), the
dirigent asks you with `request_secret`. You see a card in its chat: **Session … needs NAME**, with
why.

- **Provide** opens a field for the value and a choice: **This session**, or **All my sessions**.
  The value goes to control, encrypted; the session gets it from its next command on.
- **Decline** says no.

Either way the dirigent learns only whether it was provided, never the value, and goes on (for
example, telling the session to retry). See [Accounts and secrets](accounts-and-secrets.md#secrets).

## Tips

- Give it the goal and the constraints, as you would a lead: which repositories, what done means,
  what not to touch.
- It parallelizes independent tasks across sessions and reuses sessions that already have the
  context.
- Its instructions are in [`player/prompts/dirigent.md`](../../player/prompts/dirigent.md).
