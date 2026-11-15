# HTTP API

control's HTTP API, as the web and the CLI use it. Through the web it's at `https://<web>/api/…`;
control itself listens on port 8090. Bodies are JSON; errors are plain text with a status.
The web serves one route of its own, unauthenticated: `GET /version`, `{"version"}`, the web's.

## Authentication

Every route but `GET /auth` and `GET /.well-known/jwks.json` needs a caller:

| Header | Accepted |
|---|---|
| `authorization: Bearer <token>` | A token control signed (audience `control`), in any mode. With Google sign-in, also a Google ID token for one of the install's clients. |
| `dirigent-user: <subject>` | In declared auth only: whoever it names, unverified. |

The web signs in with `POST /session` (a Google ID token), which sets an HttpOnly cookie, then gets
short-lived control tokens from `POST /session/token`. The CLI sends Google ID tokens.

Common statuses: `401` no caller; `403` not the session's owner; `404` no such session (or not
yours to read); `409` the session's state doesn't allow it; `422` a request it won't take; `501` the
install lacks what it needs (no link key, no runner, no ClickHouse).

Inputs to sessions (messages, interrupts, renames, answers to secret requests) aren't in this API:
they're rows written into the session's log through chlive. See [Events](events.md).

## Sessions

| Method and path | Body | Answer | Who |
|---|---|---|---|
| `GET /sessions` | | `[SessionInfo]`: the sessions you own or view | any |
| `GET /sessions/{id}` | | `SessionInfo` | owner, viewer |
| `PUT /sessions/{id}` | `NewSession` (below) | `201` made, `200` existed with the same spec; `409` another spec; `403` someone else's; `422` over the limits, a dirigent with a source, or no working ChatGPT link | you become the owner |
| `DELETE /sessions/{id}` | | `202`: deleted in the background. `409` unless archived. | owner |
| `POST /sessions/{id}/start` | | `204`. `409` archived, being imported, or on a host that isn't connected. | owner |
| `POST /sessions/{id}/stop` | `{"now": bool}` (optional, default false) | `204` | owner |
| `POST /sessions/{id}/archive` | | `204`. `409` while a player holds it. | owner |
| `POST /sessions/{id}/unarchive` | | `204` | owner |
| `PUT /sessions/{id}/environment` | `{"host": "<name>" \| null}` | `204`: where it runs from its next start (`null`, the cluster) | owner |
| `PUT /sessions/{id}/model` | `{"model", "effort", "provider"}` (`effort` `null`: no thinking; `provider` default `anthropic`) | `204`: its player, if one runs, takes them before its next model call; otherwise its next one starts with them. `422` no model, or no working ChatGPT link | owner |
| `PUT /sessions/{id}/viewers/{subject}` | | `204` | owner |
| `DELETE /sessions/{id}/viewers/{subject}` | | `204` | owner |
| `POST /sessions/{id}/import` | | `{"token"}`: a chlive token to write an imported conversation. `409` once the session has run. | owner |
| `DELETE /sessions/{id}/import` | | `204`: the import is done; players may take the session. | owner |

`NewSession`:

```json
{
  "model": "claude-sonnet-5",
  "effort": "medium",
  "provider": "anthropic",
  "part": "coder",
  "source": {"git": {"url": "https://github.com/you/app", "ref": "main"}},
  "resources": {"cpu": 4, "memory": 8, "disk": 40},
  "viewers": ["user:someone@example.com"],
  "environment": "adams-macbook",
  "start": true
}
```

Every field is optional: `model` defaults to `claude-sonnet-5`, `effort` to `medium` (`null` for no
thinking), `provider` to `anthropic`, `part` to `coder`, `source` to none. `resources` (cores, GiB,
GiB) may be partial; what's left out is the default. `environment` names one of your hosts.

`SessionInfo`:

```json
{
  "id": "…", "spec": { … }, "epoch": 3,
  "player": {"subject": "player:…", "attached": true},
  "owner": "user:you@example.com", "viewers": [], "role": "owner",
  "created": 1794700000000, "secrets": ["GITHUB_TOKEN"],
  "archived": null, "environment": null
}
```

`player` is null when idle; `attached` false when the player is away. Times are milliseconds since
the Unix epoch. `secrets` are names: the session's own and its owner's.

## Secrets

| Method and path | Body | |
|---|---|---|
| `GET /secrets` | | Your secrets' names. |
| `PUT /secrets/{name}` | `{"value": "…"}` | `204`. `400` for a bad name (`[A-Z_][A-Z0-9_]{0,63}`) or a value empty or over 64 KiB. |
| `DELETE /secrets/{name}` | | `204` |
| `GET /sessions/{id}/secrets` | | A session's own secrets' names (owner). |
| `PUT /sessions/{id}/secrets/{name}` | `{"value": "…"}` | As above (owner). |
| `DELETE /sessions/{id}/secrets/{name}` | | (owner) |

## Accounts

| Method and path | Body | |
|---|---|---|
| `GET /me` | | `{"subject", "chatgpt": {"email", "broken"} \| null, "anthropic": {"hint"} \| null}` |
| `PUT /links/anthropic` | `{"key": "sk-ant-…"}` | `{"hint"}` (its last four characters). `400` if Anthropic refuses it. |
| `DELETE /links/anthropic` | | `204` |
| `POST /links/chatgpt/start` | `{"port": n}` | `{"url", "state"}`: where to sign in, redirecting to `http://127.0.0.1:<port>/auth/callback`. |
| `POST /links/chatgpt/finish` | the callback's query (`code`, `state`) | `{"email", "broken"}` |
| `DELETE /links/chatgpt` | | `204` |
| `GET /links/chatgpt/models` | | `[{"slug", "display_name"}]` |
| `GET /links/github` | | `{"app": bool, "accounts": ["login"]}` |
| `POST /links/github/start` | `{"existing": bool}` | `{"url"}`: install the App, or (`existing`) authorize it for installations made already. `404` without an App. |
| `GET /links/github/callback` | | GitHub's redirect back; then to `/?github=connected` or `/?github_error=…`. |
| `DELETE /links/github` | | `204` |
| `GET /github/access?url=<repo>` | | `{"repo", "via": "public" \| "token" \| "app" \| null, "why", "app"}`: whether your sessions can read it, and how. |

## Settings and the install

| Method and path | | |
|---|---|---|
| `GET /settings` | | `{"events": "conversation" \| "activity" \| "all", "machine"?: {"cpu", "memory", "disk"}}` |
| `PUT /settings` | the whole settings | `machine` only where users have their own defaults. |
| `GET /machines` | | `{"defaults", "limits", "per_user", "warm"}` |
| `GET /environments` | | `{"cluster": Machines \| null, "hosts": [{"name", "os", "arch", "cpus", "memory", "labels", "slots", "version", …}]}`: your connected hosts |
| `GET /system` | | `{"configured", "state"?: "building" \| "ready" \| "failed" \| "broken", "store_path"?, "error"?}` |
| `GET /about` | | What runs: `{"control": {"version", "started", "runner"}, "players": {"vm", "system"}, "postgres", "clickhouse", "pods": [{"name", "component", "images", "phase", "ready", "restarts", "started"}] \| null, "errors"}`. `pods` (outside players' VMs and finished Jobs) only in Kubernetes. What control couldn't read within 5 seconds is null, and `errors` say why. |

## Sign-in and tokens

| Method and path | | |
|---|---|---|
| `GET /auth` | unauthenticated | `{"mode": "declared"}` or `{"mode": "oidc", "google_client_id", "cli": {"client_id", "client_secret"} \| null}` |
| `POST /session` | `authorization: Bearer <Google ID token>` | `{"subject", "token"}` and the sign-in cookie. |
| `POST /session/token` | the cookie | `{"subject", "token"}`: a fresh control token (also valid for chlive). |
| `DELETE /session` | the cookie | Sign out. |
| `POST /token` | declared auth only | `{"token"}`: a chlive token for the caller. |
| `GET /.well-known/jwks.json` | unauthenticated | control's public keys. |

Internal routes (`POST /system/builds/{id}` for build VMs, `POST /nix/paths` for players) and the
players' gRPC service (port 8091: `Lease`, `Wait`, `Host`) are for dirigent's own parts.
