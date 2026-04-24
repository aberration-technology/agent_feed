# agent_feed ✨🎞️

`agent_feed` turns local coding-agent activity into a projection-safe feed.

agent activity, reduced to signal.

![lowercase feed projection showing a settled codex file-change bulletin](docs/image/hero.png)

the browser surface follows the root aberration style: black field, white type,
secondary accent links, and lowercase chrome.

core shape:

* adapters observe codex, claude, mcp, shell, and telemetry surfaces
* events are normalized before storage
* redaction runs before display or publish
* story windows settle noisy streams into contextual headlines
* publisher-side summaries reduce bandwidth, token cost, and data exposure
* the projection surface advances on its own; no scrolling, no controls, no cloud

the product is not a dashboard. it is a local broadcast layer.

## install

```sh
cargo install agent_feed_cli --locked
agent-feed init --auto
agent-feed serve
agent-feed open
```

the local projection page is served at:

```text
http://127.0.0.1:7777/reel
```

## local capture

`agent_feed` works with future and active agent sessions when those sessions expose
a transcript, stream, hook, app-server, or telemetry surface.

```sh
agent-feed codex active --sessions 2 --watch
agent-feed claude active --sessions 2 --watch
```

use explicit imports when a stream or transcript is already available:

```sh
agent-feed ingest --source codex-jsonl < events.jsonl
agent-feed ingest --source claude-stream-json < events.jsonl
agent-feed codex import ~/.codex/sessions/2026/04/23/rollout-...jsonl
agent-feed claude stream < stream.jsonl
```

hook helpers are telemetry by default. they fail open and do not block the
agent unless installed as policy hooks:

```sh
agent-feed hook --source claude --event PreToolUse
```

## safety boundary

raw prompts, secrets, absolute home paths, command output, and file diffs are
not display material by default.

default posture:

* bind to `127.0.0.1`
* no cloud
* no analytics
* raw event storage off
* aggressive redaction on
* path hashing on
* non-loopback bind requires a display token

invariants:

* redaction runs before store, SSE, and p2p publish
* query params cannot disable redaction
* raw-event quarantine is local-only
* hook helpers never print secrets
* adapter errors do not panic the daemon

the feed is a view of agent activity, not an agent control plane.

## commands

```text
agent-feed doctor
agent-feed init --auto
agent-feed serve
agent-feed serve --p2p
agent-feed open

agent-feed auth github
agent-feed auth status
agent-feed auth logout

agent-feed codex active --sessions 2 --watch
agent-feed codex stories --sessions 2
agent-feed claude active --sessions 2 --watch
agent-feed claude stories --sessions 2

agent-feed p2p init
agent-feed p2p join mainnet
agent-feed p2p share --feed-name workstation --visibility private
agent-feed p2p publish --dry-run --agents codex,claude --sessions 2
agent-feed p2p publish --dry-run --summarizer deterministic
```

`--feed`, `--feed-name`, and `--feed-label` are aliases. all selected local
agent sessions in a publish run are bundled under that logical feed name.

## github auth

native github auth is edge-mediated. the cli never needs a github client secret.

```sh
agent-feed auth github
```

the cli binds a one-shot loopback callback, opens the edge sign-in URL, validates
the returned `state`, and writes the profile/session to
`~/.agent_feed/auth/github.json` with owner-only permissions.

useful variants:

```sh
agent-feed auth github --print-url
agent-feed auth github --no-browser
agent-feed auth github --edge https://edge.feed.aberration.technology
```

the hosted browser uses the same edge authority. `/network` is the interactive
surface for sign-in, private feed grants, and browser seed material.

## story compiler

the display path is:

```text
event -> story window -> settled story -> bulletin
```

low-context token streams, shell polling, session starts, and file reads are
stored but not shown. a story settles on changed files, failed tools, failed
turns, permission events, test signals, plan updates, or a final flush.

## summarization

summarization is publisher-side. subscribers receive already-summarized story
capsules and do not spend tokens or run external processors.

the cli publisher defaults to codex-backed aesthetic headline summarization:

```sh
agent-feed p2p publish --dry-run --summary-style "austere technical broadcast; release-room concise"
```

use deterministic mode for offline or zero-token publishing:

```sh
agent-feed p2p publish --dry-run --summarizer deterministic
```

supported processors:

```text
codex-exec
claude-code
deterministic
process
http-endpoint
```

external processors receive only redacted story facts. their output is
guardrailed again before a capsule is signed. processors may return
`publish=false` when the headline has not meaningfully changed.

headline images are optional and disabled by default:

```sh
agent-feed p2p publish --dry-run --images --image-processor codex-exec
```

viewers stay text-only unless they opt in with `?images=on` or local storage key
`agent_feed.images=enabled`.

## p2p

p2p is opt-in. local mode remains the default.

```text
agent streams -> redaction -> story compiler -> signed story capsules -> p2p feed
```

the p2p layer publishes settled story capsules, not raw events. private feeds
require subscription grants. public topic names hash feed ids and do not include
handles, repo names, hostnames, or local usernames.

network participation is not a feed subscription. native peers may cache
directory records, route rendezvous/kad lookups, or host browser handoff
transports without receiving story capsules. a feed starts delivering only after
an explicit follow or grant path.

## github routes

the hosted browser shell treats a single path segment as a github user lookup:

```text
https://feed.aberration.technology/mosure
https://feed.aberration.technology/@mosure
https://feed.aberration.technology/mosure/*
https://feed.aberration.technology/mosure/workstation
https://feed.aberration.technology/mosure?all
https://feed.aberration.technology/mosure/workstation?view=timeline
```

the path login is a human-readable alias. the edge resolves it to the durable
github numeric user id, then issues a signed discovery ticket with the verified
profile, visible feed records, bootstrap peers, rendezvous namespaces, provider
keys, and a signed browser seed.

`mosure/*` and `?all` mean all visible settled story streams. they do not mean
raw stream access.

projection routes are automated by default. interactive browsing is opt-in:

```text
?view=timeline
?timeline=1
```

timeline mode is a vertical, scroll-snapped view over the selected user/feed
with a bounded ring buffer of recent settled story capsules.

## deployment

production uses a split-host model:

```text
feed.aberration.technology
  browser shell and username deep links through the edge

edge.feed.aberration.technology
  github auth, browser seeds, directory, rendezvous/bootstrap edge
```

the Pages workflow publishes the static browser shell. the AWS workflow manages
the edge EC2 host, Route53 records, Caddy TLS/proxying, SSM-backed OAuth
material, and live canaries.

operator docs live in:

```text
crates/agent_feed_p2p/deploy/README.md
```

## workspace map

```text
agent_feed/
  Cargo.toml
  README.md
  LICENSE-APACHE
  LICENSE-MIT
  deny.toml
  justfile
  xtask/

  crates/                     primary crates
    agent_feed/                 public facade
    agent_feed_cli/             binary: agent-feed
    agent_feed_core/            ids, event model, typed errors
    agent_feed_adapters/        codex, claude, mcp, telemetry adapters
    agent_feed_ingest/          HTTP and JSONL ingest
    agent_feed_redaction/       secret scanning and path masking
    agent_feed_story/           story windows and settle rules
    agent_feed_summarize/       feed rollups and external processors
    agent_feed_reel/            bulletin scheduling
    agent_feed_server/          axum routes, SSE, embedded UI
    agent_feed_ui/              self-contained browser client
    agent_feed_directory/       route parser and signed directory records
    agent_feed_p2p_proto/       signed profiles, capsules, grants
    agent_feed_p2p/             native publish/subscribe boundary
    agent_feed_p2p_browser/     browser route states and view models
    agent_feed_edge/            auth, directory, browser seed edge
    agent_feed_testkit/         fake streams and fixtures
```

## development

```sh
cargo xtask check
just check
```

`cargo xtask` is a workspace alias for `cargo run -p xtask --`.
