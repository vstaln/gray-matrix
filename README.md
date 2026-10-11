# gray-matrix

A standalone Matrix channel adapter for [gray](https://github.com/vstaln/gray)
— `/sync` long-poll in, `m.room.message` replies out, with its own setup
wizard, pairing allowlist, durable SQLite queue, and a background service.
Same shape as `gray-discord-plugin`/`gray-irc`, Matrix Client-Server REST
underneath (ported from hermes `plugins/platforms/matrix` — raw reqwest,
no matrix-sdk).

**Status: v1.** Inbound `m.room.message` events from allowlisted users in
allowlisted rooms run one isolated `gray` turn per conversation; outbound
is `m.text` + `org.matrix.custom.html`. See
[runtime policy and limits](docs/RUNTIME.md).

## Requirements

- `gray` on PATH, provider/model configured in `~/.gray/config.json`
- A Matrix account on any homeserver (matrix.org, Synapse, Conduit, ...)
- Plain (unencrypted) rooms — E2EE is not supported

## Install

```sh
cargo install --path .          # puts gray-matrix on PATH
gray plugin install matrix      # or let gray find the binary
```

## Setup

```sh
gray matrix setup               # or: gray-matrix setup
```

The wizard asks for:

- homeserver base URL (`https://` added when missing)
- an existing access token, or the account password for a `/login`
  exchange (the password is never stored; the returned token is)
- rooms the bot may answer in (comma-separated `!id:server`/`#a:server`)
- Matrix user IDs allowed to trigger the bot
- home room for notices (default: first room)
- free-response rooms where no mention is needed (default: none)

It saves the config privately (`~/.config/gray-matrix/config.json`, mode
0600), registers the `matrix_send` tool with gray, probes the token
(`whoami`) and room resolution (doctor), and offers to install the
background service.

The token can also come from `MATRIX_ACCESS_TOKEN` instead of the config
file.

## Running

```sh
gray matrix status      # service health
gray matrix stop        # graceful stop (posts shutdown notices)
gray matrix restart
gray matrix uninstall   # service only; config and sessions stay
gray matrix run         # foreground daemon (what the service execs)
gray matrix doctor      # verify token + local gray config
gray matrix queue list / cancel <id>
gray matrix allowlist add|remove|list
gray matrix pairing approve matrix <CODE>
gray matrix limits --timeout-seconds 600 --concurrency 2 --max-requests 200
```

The service probes what this box actually supervises with (runit,
`systemd --user`, or a detached pidfile daemon when no init is available).

## Talking to it

- DM the bot (invite it with "direct chat" checked — it auto-joins), or
  mention it in a joined allowlisted room: `@bot:server your message`.
- `/new` or `/reset` in a room starts a fresh session.
- `/steer <text>` adds guidance to the turn already running in that conversation ("Steering the current turn."); with nothing running it is sent as a new message.
- `/stop` (or `!stop`) cancels the turn running in that conversation ("Stopped."); queued messages still run next. With nothing running it answers "Nothing running."
- Messages sent while a turn runs queue as their own next turn ("Queued — N ahead.", at most once per 30s). Set `busy_input_mode: "steer"` in the config to steer the running turn instead.
- Long command output is paged: page 1 ends with `page 1/N — /page 2`; `/page N` shows page N of the conversation's last paged output.
- A DM from an mxid that is not allowlisted gets a one-shot pairing code;
  the owner approves it with `gray matrix pairing approve matrix <CODE>`.
- Room invites from allowlisted users are auto-joined; all others are
  ignored.

## Tools (registered with gray)

- `matrix_send {content, room?}` — markdown rendered to
  `org.matrix.custom.html`, chunked at 8,000 chars; room defaults to the
  home room.

## Limits (v1)

- Text only: no media upload/download, reactions, threads, or edits
  (an edit is never a new turn).
- No E2EE: encrypted rooms are not read or written.
- In a shared room the bot only answers when mentioned; it ignores room
  traffic not meant for it.
- Delivery is at-least-once: a homeserver outage between agent finish and
  the send landing retries the same answer rather than regenerating.
- One daemon per config (`gateway.lock`); two daemons would fight over
  the sync cursor.
