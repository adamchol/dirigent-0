# Events

A session's log is a sequence of rows, one per event, in the Kafka topic `session_events` and the
ClickHouse table `dirigent.session_events`. chlive serves it live; anything that can read a session
can follow it, and the session's owner (and its player) write to it.

## Rows

| Column | Type | |
|---|---|---|
| `session_id` | UUID | The Kafka message key, so a session's events stay in order in one partition. |
| `offset`, `partition` | integers | The message's place in Kafka: a session's events are ordered by `offset`. |
| `ts` | DateTime64(3), UTC | When Kafka got it. |
| `id` | UUID | The event's own id (inputs are referred to by it). |
| `author` | string | Who wrote it, set by chlive: `user:<email>`, `player:<id>`, `import:<session>`. |
| `kind` | string | The event's kind (below). |
| `data` | JSON string | Its fields. |

Rows of kind `model_delta` (streamed text) are kept one day; the finished reply is in
`assistant_message`. Rows of kind `files_changed` carry workspace bundles and are read apart from
the rest.

Who may write which kinds is enforced by chlive: owners write `user_message`, `user_interrupt`,
`titled` and `secret_answered`; players write any kind; viewers write nothing.

## Kinds

### Players

| Kind | Fields | |
|---|---|---|
| `player_attached` | `epoch`, `version`, `environment`?, `settings`? | A player took the session, as its `epoch`-th; on a host, `environment` names it. `settings`: the `model`, `effort` and `provider` it runs. |
| `player_detached` | `epoch` | It stopped and gave the session back. |
| `player_failed` | `epoch`, `error` | It failed and gave the session back. |

### The workspace

| Kind | Fields | |
|---|---|---|
| `workspace_ready` | `source`?, `head`?, `recreated`, `files`? | The workspace is in place: cloned from `source` at commit `head`; `recreated` if it had been found gone; `files`, the log's commit checked out over it. |
| `workspace_restored` | `snapshot` | Restored from a checkpoint. |
| `checkpoint` | `turn`, `snapshot` | Checkpointed after a turn. |
| `checkpoint_failed` | `turn`, `error` | |
| `files_changed` | `commit`, `parent`?, `part`, `parts`, `bundle` | A workspace commit: a git bundle (base64) of what's new since `parent`, in parts of up to 512 KiB. |
| `secrets_changed` | `names` | The secrets the session's tools get changed (names only). |
| `model_changed` | `settings` (`model`, `effort`, `provider`) | The session was switched; the player takes them from its next model call. The model isn't told. |

### Inputs

| Kind | Fields | |
|---|---|---|
| `user_message` | `text`, `via`? | A message. `via`: the dirigent session that sent it. |
| `user_interrupt` | `turn` | Interrupt that turn. |
| `session_update` | `session`, `offset`, `update`, `reply`? | (A dirigent's log.) A session it follows finished a turn, failed or stopped; `reply` is its last. |
| `secret_answered` | `session`, `name`, `provided`, `call` | (A dirigent's log.) The owner provided or declined a requested secret; never its value. |

### Turns

| Kind | Fields | |
|---|---|---|
| `turn_started` | `input` | A turn began, from the input row `input`. |
| `steered` | `input` | The input `input` joined the running turn. |
| `model_delta` | streamed deltas | Part of a reply as it streams. |
| `assistant_message` | `content`, `stop_reason`, `usage` | A finished reply: text, thinking and tool calls, and its token usage (input, output, cache reads and writes). |
| `model_cancelled` | | A model call was cancelled. |
| `tool_result` | `call_id`, `name`, `content`, `is_error`, `meta` | A tool's result, secrets redacted. |
| `error` | `message`, `retry_in_ms`?, `compact`? | A model call failed: it will be retried in `retry_in_ms`, or the conversation compacted (`compact`), or neither (the turn fails). |
| `turn_ended` | `reason`, `interrupt`? | `completed`, `aborted` (`interrupt`: the interrupt that did it) or `failed`. |

### The conversation

| Kind | Fields | |
|---|---|---|
| `compacted` | `summary`, `first_kept`, `usage` | Everything before the event `first_kept` is replaced by `summary`. |
| `compaction_failed` | `message` | |
| `titled` | `title`, `description`, `hold`? | The session's name; `hold` (the owner's) stops the agent renaming it. |
| `imported` | `from`, `session`, `cwd`, `first`, `last` | The conversation before it came from another agent's session (`claude-code`, `codex`), from `first` to `last`. |

The exact shapes are the `Event` type in [`protocol/src/lib.rs`](../../protocol/src/lib.rs); how a
log reads as a conversation is [`protocol/src/fold.rs`](../../protocol/src/fold.rs).
