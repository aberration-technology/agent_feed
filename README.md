# agent_feed ✨🎞️

`agent_feed` turns local coding-agent activity into a projection-safe feed.

agent activity, reduced to signal.

![lowercase feed projection showing a settled codex file-change bulletin](docs/image/hero.png)

## what this is

`agent_feed` is a local Rust daemon, CLI, and browser surface for watching
coding agents without showing raw logs.

the default product is local:

```text
agent streams -> redaction -> story compiler -> local feed
```

it observes codex, claude, mcp, hooks, transcripts, JSONL streams, and generic
telemetry surfaces. events are normalized, redacted, grouped into settled
stories, then rendered as sparse feed bulletins.

the screen is meant to be left alone. no scrolling, no dashboard controls, no
raw prompt/output/diff display.

## getting started

```sh
cargo install agent_feed_cli --locked
agent-feed init --auto
agent-feed serve
agent-feed open
```

the local feed is served at:

```text
http://127.0.0.1:7777/reel
```

for active local sessions:

```sh
agent-feed codex active --sessions 2 --watch
agent-feed claude active --sessions 2 --watch
```

to opt in only one workspace, add `--workspace /path/to/repo` to the codex,
claude, or p2p publish commands. events without a matching `cwd` are ignored
before import, story compilation, or p2p publishing.

for an existing transcript or stream:

```sh
agent-feed codex import path/to/codex-session.jsonl
agent-feed claude import path/to/claude-stream.jsonl
```

that is enough for the normal local loop: start the daemon, attach future or
active agent activity, and leave the browser feed open.

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
* query params cannot weaken privacy

the feed is a view of agent activity, not an agent control plane.

## p2p mode

p2p is optional. local mode remains the default.

```sh
agent-feed serve --p2p
agent-feed p2p share --feed-name workstation --visibility private
```

p2p publishes signed, settled story capsules. it does not publish raw local
events by default. subscribers receive already-summarized feed material.

the hosted browser shell is:

```text
https://feed.aberration.technology/
```

user paths resolve github usernames through the edge, then subscribe only to
visible settled story streams:

```text
https://feed.aberration.technology/mosure
https://feed.aberration.technology/mosure/*
https://feed.aberration.technology/mosure/workstation
```

## repo shape

this is a Rust workspace with narrow crates for the CLI, local server, adapters,
redaction, story compilation, summarization, browser UI, p2p protocol/runtime,
edge support, and test fixtures.

most contributors should start with:

```sh
cargo xtask check
```

`cargo xtask` is a workspace alias for `cargo run -p xtask --`.

## license

licensed under either of:

* apache license, version 2.0
* mit license
