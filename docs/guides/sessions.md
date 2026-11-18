# Sessions

How to make sessions and work with them. The web does all of this; the same actions are in the
[HTTP API](../reference/http-api.md), and a [dirigent](dirigents.md) can do most of them for you.

## Making a session

In the web: **New session** (see [The web UI](web.md#making-a-session)). Through the API:

```sh
curl -X PUT https://dirigent.example.com/api/sessions/$(uuidgen) \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"model": "claude-sonnet-5", "effort": "high",
       "source": {"git": {"url": "https://github.com/you/app", "ref": "main"}},
       "start": true}'
```

You choose the id. Making a session twice with the same spec is fine (`200`, nothing changes); with
another spec it's refused (`409`). The caller becomes the owner.

### Model, provider and thinking

- **Provider `anthropic`** (default): calls go to Anthropic's API with *your* API key (add it in the
  web's account menu or with `dirigent link anthropic`). The model is any model id Anthropic serves,
  e.g. `claude-sonnet-5`, `claude-opus-5-5`, `claude-haiku-5-5`.
- **Provider `chatgpt`**: calls go through your ChatGPT plan, linked with `dirigent link chatgpt`.
  The model is one of your plan's (`dirigent models` lists them). Making a ChatGPT session needs a
  working link.
- **Effort** is how much the model thinks before answering: `low`, `medium` (default), `high`, `max`,
  or none (`null`). Models that can't stop thinking (Claude Opus and Sonnet 5.5 and later, Fable;
  GPT's Astra and Sol, GPT-6.2 Astra among them) run "none" at their lowest effort instead.

The context window is 200k tokens on Anthropic and 272k on ChatGPT; long sessions are
[compacted](agent.md#compaction) before they reach it.

### Switching model, provider or thinking

A session's model, effort and provider can change while it lives: **Model** in the session menu, the
dirigent's `update_session`, or `PUT /sessions/{id}/model` with `{"model", "effort", "provider"}`. The
conversation goes on whole.

- **Running**, it takes them from its next model call; a call running finishes as it began. The log
  says so (`model_changed`), and the chat shows it. **Idle**, its next player starts with them.
- The next call reads the whole conversation afresh, without the provider's cache, once: on a long
  session that's one call's worth of full-price input. Changing only the effort does too on
  Anthropic.
- Thinking doesn't cross providers: switched from Anthropic to ChatGPT (or back), the model gets the
  conversation without the other's reasoning. Between Claude models it all goes back.
- A smaller context window (Anthropic's 200k after ChatGPT's 272k) compacts the conversation first
  if it no longer fits.
- Switching to a ChatGPT plan needs a working link, as making a session on one does.

### Source

A git URL (and optionally a ref) is cloned into the workspace when the first player starts, as a
partial clone (`--filter=blob:none`) of that ref, or of the default branch without one. Private
GitHub repositories need a token or the GitHub App; see [Accounts and
secrets](accounts-and-secrets.md#github); with either, `git@github.com:` URLs are fetched over
https. No source means an empty workspace. A dirigent has none.

### Machine

On the cluster, a session runs on the **default machine** (the install's, or your own default if
the install lets users set one) as it is *when the session starts*, unless it asks for its own:
cores, memory (GiB) and workspace disk (GiB), within the install's limits. A session that asked for a
machine keeps it. See [Machines and systems](../operating/machines-and-systems.md).

Its owner can change it later (**Machine** in the web's session menu, the dirigent's `update_session`, or `PUT /sessions/{id}/machine`
with the parts to change). An idle session's next start runs on the new machine. An active one
moves: its player stops once the turn running is done, and the next starts on the new machine at
once, its workspace restored from its last checkpoint (taken after each turn). A stop meanwhile keeps it
stopped. The disk only grows: each VM's workspace disk is new, filled from the checkpoint, which a
smaller one might not hold. A session placed on a host has that host's machine and can't change it.

## Talking to it

- **Messages** start turns. Send one in the web, or have a dirigent send it. An idle session is
  started by a message.
- **Steering**: a message sent during a turn joins it. The model reads it at its next step (after
  the tools it's running finish).
- **Queued messages**: messages sent while a session is idle or stopping wait in the log; each gets
  a turn, in order, once a player is there.
- **Interrupting** (the composer's **Stop** button) ends the running turn at once: the model call
  is cancelled and running commands are killed. The player stays and the session waits for your
  next message, which the model reads knowing the previous turn was interrupted.

## Starting and stopping

- **Start** (`POST /sessions/{id}/start`, or **Start** in the web) starts a player for an idle
  session. Sending a message does it too.
- **Stop** (`POST /sessions/{id}/stop`): the player starts no new turn, lets the running one finish
  (and checkpoint), then leaves. The session is idle.
- **Stop now** (`{"now": true}`): the running turn is cancelled first.

Messages that arrive while a session is stopping stay queued for its next start.

If a session's player is lost (its VM was deleted, its machine died), control starts another, which
continues from the log; a turn that was cut off is closed as aborted and the model is told so. A VM
being drained gets time to finish its turn (up to the install's stop grace, an hour by default)
before it hands over.

## Sharing

Add viewers in the web's **Viewers** popover, or with `PUT /sessions/{id}/viewers/user:someone@example.com`.
Viewers read the session live and can follow its workspace with `dirigent sync`. Only the owner
(and the owner's dirigents) write to a session, start or stop it.

## Renaming

The agent names a session after its first message (a title of a few words and a one-sentence
description), and again after each compaction as the work moves on. **Rename** in the web sets your
own; with **Keep this name** on, the agent stops renaming it until you turn it off.

## Moving it

A session runs on the cluster, or on one of your [hosts](hosts.md). **Runs on** in an idle session's
menu (or `PUT /sessions/{id}/environment {"host": "name"}`, `null` for the cluster) moves it for its
next start. It takes its conversation and files along; the agent is told it moved.

## Archiving and deleting

- **Archive** an idle session to put it away: it keeps everything, but no player takes it and no one
  can write to it. **Unarchive** brings it back.
- **Delete** an archived session to remove it for good: its events (from ClickHouse), its
  checkpoints and its secrets. Deletion runs in the background and takes about a minute. The Kafka
  topic keeps the raw messages until its retention (90 days by default) runs out.

## Limits and errors

| When | You get |
|---|---|
| Starting a session placed on a host that isn't connected | `409`: the session runs on *name*, which isn't connected |
| Starting an archived session | `409` |
| A machine over the install's limits | `422` |
| A ChatGPT session without a working link | `422` |
| Writing to a session you only view | refused by chlive; `403` from control |
| No Anthropic key when a turn starts | the turn fails: "no Anthropic API key: add yours in the web's account menu, or with `dirigent link anthropic`" |
