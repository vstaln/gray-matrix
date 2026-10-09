# Runtime notes

Requires a gray core implementing `gray -p --json` protocol 1 (the same
`{"protocol":1,"turn_id",...}` NDJSON the discord/slack adapters consume).

## Transport

Plain Matrix Client-Server API over HTTPS via reqwest + rustls (no openssl,
no matrix-sdk — the surface is ~8 endpoints and the SDK would pull in the
E2EE stack this bridge deliberately omits). `Authorization: Bearer` from a
password `/login` (wizard) or a pasted access token; `whoami` verifies it.
Inbound is `GET /sync` long-poll (30 s hold, 45 s client timeout) with a
persisted `since` cursor; `M_LIMIT_EXCEEDED` is honored via
`retry_after_ms`, and 5xx/network errors reconnect with bounded backoff
(1 s → 60 s). Every request is bounded so a wedged socket cannot freeze
the loop.

Endpoints used: `POST /login`, `GET /account/whoami`, `GET /sync`,
`PUT /rooms/{id}/send/m.room.message/{txn}`, `PUT /rooms/{id}/typing/{u}`,
`POST /join/{roomIdOrAlias}`, `GET /directory/room/{alias}`,
`GET /joined_rooms`.

## Inbound policy

- The bot never answers itself; `m.room.message` only, `m.text` and
  `m.emote` msgtypes (notices and media carry no prompt). Edits
  (`m.replace`) and the quoted reply fallback are dropped.
- Admission: the sender's mxid must be on `allowed_users`, and the room
  must be on `rooms` (aliases resolve to ids at boot). DMs are exempt from
  the room allowlist but not the user allowlist.
- In a shared room the bot only answers when mentioned
  (`m.mentions.user_ids` or a textual `@bot:server`), unless the room is
  in `free_response_rooms` or `require_mention` is false. DMs always turn.
- Invites: auto-joined when the inviter is allowlisted, or when the invite
  is `is_direct` (that is how pairing starts). Other invites are ignored.
- An un-admitted DMer gets a pairing reply (code minted in SQLite);
  `gray matrix pairing approve matrix <code>` adds the mxid. Matrix user
  ids are homeserver-authenticated, unlike IRC nicks.
- Conversation keys: `room:<room_id>` (shared room transcript) or
  `dm:<sender mxid>`.
- Session reset policy `session_reset` (`both`: 1,440 idle minutes or the
  next 04:00 local boundary) mirrors the other adapters; `/new` and
  `/reset` force it early.

## Cursor and queue

The `since` token is written to the `meta` table after each batch, so a
restart replays at most the last batch; the inbox row id `mx:<event_id>`
dedupes that replay. The very first sync (no cursor) only captures
`next_batch` and `m.direct` — backlog is never turned into turns.

Accepted messages commit to `matrix.sqlite` next to the config before any
agent work: `queued` → `running` → `done`/`failed`/`cancelled`. Boot
re-queues interrupted `running` rows and reports the count in the startup
notice. `concurrency` workers (default 2, max 16) claim items; a
per-conversation flock serializes turns inside one room. `queue list`
shows pending work; `queue cancel <id>` cancels what is still queued.

## Delivery

Replies are `m.text` events with `org.matrix.custom.html` formatted bodies
(markdown rendered via pulldown-cmark; raw HTML in model output is shown
literally, never passed through). A generated answer retries delivery
(bounded) across a homeserver outage rather than regenerating — delivery
is at-least-once, never exactly-once. A typing notice marks a running
turn, best-effort.

`matrix_send` is a standalone resolve-and-send, not an outbox transaction —
its errors must not be blindly retried.

## Limits

`gray matrix limits --timeout-seconds 600 --concurrency 2 --max-requests 200`
sets turn wall time, worker count, and per-turn provider requests; restart
to apply. `queue_capacity` (default 1,000) bounds the inbox; a full queue
tells the sender instead of dropping silently.

## Lifecycle notices

`lifecycle.json` + `restart_pending` next to the config, worded by
`gray gateway lifecycle boot|stop` when the installed gray has it (local
fallback otherwise). Rooms with a running turn hear the shutdown/restart
notice; the home room hears the boot notice after the first sync.
`"restart_notification": false` silences it.

## E2EE

Out, deliberately. The bridge does not implement olm/megolm or key
verification; encrypted rooms are not joined, read, or written. Deploy
against plain rooms (and document to users that room history is visible
to the homeserver).
