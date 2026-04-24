# agent_reel ✨🎞️

`agent_reel` turns local coding-agent activity into a projection-safe live reel.

agent activity, reduced to signal.

![feed projection showing a redacted codex file-change bulletin](docs/image/hero.png)

core shape:

* adapters observe codex, claude, mcp, shell, and telemetry surfaces
* events are normalized before storage
* redaction is applied before display
* story windows settle noisy streams into contextual bulletins
* highlight scoring prefers completion, failure, risk, novelty, and changed state
* the browser surface advances on its own; no scrolling, no controls, no cloud

the product is not a dashboard. it is a local broadcast layer.

## install

```sh
cargo install agent_reel_cli --locked
agent-reel init --auto
agent-reel serve
```

## happy path

```sh
agent-reel open
```

the reel is served at:

```text
http://127.0.0.1:7777/reel
```

after setup, the page requires no scrolling, clicking, refreshing, or human control.
new agent activity becomes a sequence of display-safe bulletins that advance through time.

## inputs

first-class inputs:

* codex `exec --json`
* codex app-server streams
* codex hooks
* claude code `--output-format stream-json`
* claude code hooks
* mcp JSON-RPC streams
* generic local telemetry

the daemon may discover active tools, but it is honest about capture limits.
already-running private TUI sessions are only observable when they expose a stream, hook,
app-server, telemetry surface, transcript, or were launched through an `agent_reel` shim.

## safety boundary

raw prompts, secrets, absolute home paths, command output, and file diffs are not display
material by default.

default posture:

* bind to `127.0.0.1`
* no cloud
* no analytics
* no raw prompt display
* no raw command output display
* no raw diff display
* raw event storage off
* aggressive redaction on
* path hashing on
* non-loopback bind requires a display token

security invariants:

* redaction runs before store
* redaction runs before SSE
* query params cannot disable redaction
* raw-event quarantine is local-only
* uninstall restores edited config files
* hook helper never prints secrets
* adapter errors never panic the daemon

the reel is a view of agent activity, not an agent control plane.

## quick ingest

start the daemon:

```sh
agent-reel serve
```

send a safe generic event:

```sh
curl -sS http://127.0.0.1:7777/ingest/generic \
  -H 'content-type: application/json' \
  -d '{
    "agent": "codex",
    "project": "agent_reel",
    "kind": "turn.complete",
    "title": "signal path is live",
    "summary": "generic ingest produced one redacted bulletin.",
    "tags": ["m0", "redacted"]
  }'
```

open:

```sh
agent-reel open
```

## commands

```text
agent-reel doctor
agent-reel init --auto
agent-reel init --codex
agent-reel init --claude
agent-reel serve
agent-reel open
agent-reel auth github
agent-reel auth status
agent-reel auth logout
agent-reel ingest --source generic < event.json
agent-reel ingest --source codex-jsonl < events.jsonl
agent-reel codex active --sessions 2
agent-reel codex active --sessions 2 --watch
agent-reel codex import ~/.codex/sessions/2026/04/23/rollout-...jsonl
agent-reel codex stories --sessions 2
agent-reel p2p init
agent-reel p2p join mainnet
agent-reel p2p discover github mosure --all --explain
agent-reel p2p share --feed workstation --visibility private
agent-reel p2p publish --dry-run --sessions 2
agent-reel p2p publish --dry-run --sessions 2 --per-story
agent-reel p2p publish --dry-run --summarizer codex-exec
agent-reel p2p publish --dry-run --summarizer claude-code
agent-reel p2p publish --dry-run --images --image-processor codex-exec
agent-reel p2p publish --dry-run --images --image-processor process --image-command ./summarize-image
agent-reel p2p publish --dry-run --images --image-processor http-endpoint --image-endpoint http://127.0.0.1:8787/summarize-image
agent-reel hook --source claude --event PreToolUse
agent-reel uninstall --restore-hooks
```

`codex active` reads `~/.codex/history.jsonl`, selects the most recent distinct
session ids, finds their transcript JSONL files under `~/.codex/sessions`, and
imports display-safe lifecycle/tool/file-change events through the normal
redaction path.

`codex stories` uses the same active-session discovery, but it does not post to
the daemon. it compiles selected transcripts into settled story summaries.

the hook helper reads stdin, posts to loopback, exits `0` on daemon failure, and never
blocks the agent unless explicitly installed as a policy hook. telemetry hooks are
fail-open.

## github auth

native github auth is edge-mediated. the cli never needs a github client
secret.

```text
agent-reel auth github
```

the command binds a one-shot loopback callback, opens the edge github sign-in
URL in the browser, validates the returned `state`, and writes the resulting
github profile/session to `~/.agent_reel/auth/github.json` with owner-only file
permissions.

useful variants:

```text
agent-reel auth github --print-url
agent-reel auth github --no-browser
agent-reel auth github --edge https://edge.feed.aberration.technology
agent-reel auth github --callback-bind 127.0.0.1:0
```

the hosted browser shell uses the same edge authority. `/network` is the
interactive surface for github sign-in, private feed grants, and browser seed
material. projection routes stay story-only and do not become raw stream access
after sign-in.

## story compiler

the display path is:

```text
event -> story window -> settled story -> bulletin
```

low-context token streams, shell polling, session starts, and file reads are stored but
not shown. a story settles on changed files, failed tools, failed turns, permission
events, test signals, plan updates, or a final flush. summaries are deterministic and
do not copy raw prompts, command output, or diffs.

## p2p

p2p is opt-in. the local reel remains the default.

```text
agent streams -> redaction -> story compiler -> signed story capsules -> p2p feed
```

the p2p publish layer accepts signed story capsules, not raw events. private feeds
require subscription grants. public topic names hash feed ids and do not include handles,
repo names, hostnames, or local usernames.

network participation is not a feed subscription. native peers may join the fabric,
cache signed directory records, serve rendezvous/kad/provider lookups, or host
browser handoff transports without receiving story capsules. a feed starts delivering
only after an explicit follow/subscription grant path.

summarization is publisher-side. subscribers receive already-summarized capsules and do
not spend tokens or run external processors. the default p2p publisher uses a deterministic
feed rollup, so a noisy window becomes one small recap capsule. `--per-story` keeps one
capsule per settled story when an operator wants more detail.

external summarizers are opt-in and still pass through the same guardrails before signing:

```text
deterministic
codex-exec
claude-code
process
http-endpoint
```

`codex-exec` and `claude-code` are built-in process profiles. custom processes,
HTTP endpoints, and in-process adapters receive only redacted story facts, never
raw prompts, raw command output, raw diffs, or absolute paths. their output is
guarded again before a capsule is signed.

headline images are also publisher-side and disabled by default. when enabled,
the image processor receives the settled headline, deck, lower-third, chips, and
style policy, then may return either a cached image reference or `null`. not
every headline needs art. image output is still guardrailed: no raw prompts,
readable code, command output, diffs, absolute paths, repo names, credentials,
or personal data. subscribers do not run image generation.

the projection UI is text-only by default. a viewer may opt in with
`?images=on` or local storage key `agent_reel.images=enabled`; `?images=off`,
`text=only`, or `text_only=true` force text-only mode.

## github routes

the hosted browser shell treats a single path segment as a github user lookup:

```text
https://feed.aberration.technology/mosure
https://feed.aberration.technology/@mosure
https://feed.aberration.technology/mosure/*
https://feed.aberration.technology/mosure/workstation
https://feed.aberration.technology/mosure?all
https://feed.aberration.technology/mosure?streams=workstation,release
https://feed.aberration.technology/mosure?agents=codex,claude&kinds=turn.complete,test.fail&min_score=75
https://feed.aberration.technology/mosure/workstation?view=timeline
```

the path login is only a human-readable alias. the edge resolves it to the
durable github numeric user id, then issues a signed discovery ticket containing
the verified profile, visible feed records, bootstrap peers, rendezvous
namespaces, provider keys, and a signed browser seed.

remote headlines render the publisher from the signed feed owner, not from raw
capsule text. the browser shows the cached github avatar and `@login` beside the
headline when the feed record or capsule carries a verified github publisher.

feed names are logical labels. one account may publish multiple logical feeds,
and more than one node may announce the same logical feed name:

```text
mosure/workstation
mosure/release
mosure/*
```

`mosure/*` means all visible settled story streams for that github identity. it
does not mean raw stream access.

the default route remains automated projection. interactive browsing is opt-in:

```text
?view=timeline
?timeline=1
```

timeline mode is a vertical, scroll-snapped view over the selected user/feed.
it keeps a bounded ring buffer of recent settled story capsules and never
subscribes to private feeds without an explicit grant. `/reel` and the normal
deep links stay hands-free.

`?all` means all visible settled story streams:

```text
story_only = true
raw_events = false
require_settled = true
```

privacy-weakening query params such as `redact=off`, `raw=true`, and `diffs=true`
are ignored by the route compiler.

waiting states are explicit:

```text
resolving github identity
finding feeds on mainnet
dialing p2p peers
waiting for story capsules
```

the projection page does not show raw network errors. detailed failure state
belongs in `/network/debug`.

## local api

```text
GET  /reel
GET  /reel/{view}
GET  /events.sse

GET  /api/reel/snapshot
GET  /api/bulletins
GET  /api/events
GET  /api/agents
GET  /api/sessions
GET  /api/adapters
GET  /api/health

POST /ingest/codex/jsonl
POST /ingest/codex/hook
POST /ingest/claude/stream-json
POST /ingest/claude/hook
POST /ingest/mcp
POST /ingest/otel
POST /ingest/generic
```

edge api:

```text
GET  /auth/github
GET  /callback/github
GET  /resolve/github/{login}
GET  /directory/github/{github_user_id}
GET  /directory/feed/{feed_id}
GET  /browser-seed
GET  /avatar/github/{github_user_id}
POST /subscription/request
POST /subscription/approve
GET  /network/snapshot
GET  /healthz
GET  /readyz
```

## config

```toml
[server]
bind = "127.0.0.1:7777"
public_bind_requires_token = true
snapshot_interval_ms = 5000

[store]
path = "~/.agent_reel/events.db"
raw_events = "off"
retention_days = 7
max_events = 100_000

[privacy]
mode = "aggressive"
hash_paths = true
mask_home = true
show_prompts = false
show_command_output = false
show_diffs = false

[reel]
layout = "stage"
density = "projection"
ticker = true
default_dwell_ms = 14000
urgent_dwell_ms = 20000
idle_dwell_ms = 30000
max_visible_words = 90
recap_every_secs = 420

[story]
mode = "settled"
min_score = 65
min_context_score = 70
dedupe_window_secs = 300
per_feed_max_capsules_per_min = 4

[summarize]
mode = "feed_rollup"          # feed_rollup | per_story
processor = "deterministic"   # deterministic | codex-exec | claude-code | process | http-endpoint
max_capsule_chars = 720
max_feed_rollup_stories = 32

[summarize.image]
enabled = false
processor = "disabled"        # disabled | codex-exec | claude-code | process | http-endpoint
decision = "best_judgement"   # best_judgement | always_ask | never
allow_remote_urls = false
allowed_uri_prefixes = ["/assets/headlines/", "/media/headlines/"]

[summarize.guardrails]
name = "p2p-strict"
allow_project_names = false
allow_local_paths = false
allow_command_text = false

[[summarize.guardrails.patterns]]
name = "credential"
pattern = "(?i)(password|credential|secret|token|api[_-]?key)"
action = "mask"

[p2p]
enabled = false
network_id = "agent-reel-mainnet"
profile = "local"

[p2p.discovery]
edge_directory = true
rendezvous = true
kad = true
presence_gossip = true
resolve_timeout_ms = 20000

[p2p.fabric]
enabled = true
subscribe = false              # fabric/routing participation does not imply follow
browser_handoff = true

[p2p.gossipsub]
signed_messages = true
validate_messages = true
hash_topics = true
peer_score = true

[p2p.publish]
enabled = false
summary_only = true
raw_events = false
summarizer = "deterministic"
images = false

[p2p.subscribe]
enabled = true
auto_follow_public = false
request_private_grants = true

[remote_routes]
default_mode = "settled"
allow_immediate = false
story_only = true
min_score = 75
min_context_score = 70
max_cards_per_min = 4
```

URI filters may narrow the view:

```text
/reel?agents=codex,claude
/reel?kinds=turn.complete,tool.fail,permission.request
/reel?project=burn_dragon
/reel?layout=incident
/reel?density=ambient
/reel?since=30m
```

URI filters may not weaken privacy. `?redact=off` is ignored unless the daemon is
started in explicit debug mode.

## workspace map

```text
agent_reel/
  Cargo.toml
  README.md
  LICENSE-APACHE
  LICENSE-MIT
  deny.toml
  justfile
  xtask/

  crates/
    agent_reel/           public facade, stable types, extension traits
    agent_reel_cli/       binary: agent-reel
    agent_reel_core/      ids, time, event model, score model, typed errors
    agent_reel_adapters/  codex, claude, mcp, otel, shim, process discovery
    agent_reel_ingest/    HTTP ingest, JSONL ingest, hook helper protocol
    agent_reel_redaction/ secret scanning, path masking, prompt/output policy
    agent_reel_filter/    allow/deny rules, URI filters, project filters
    agent_reel_highlight/ scoring, clustering, deduplication, recap generation
    agent_reel_story/     story windows, settle rules, p2p-safe summaries
    agent_reel_summarize/ feed rollups, guardrails, external processors
    agent_reel_reel/      bulletin scheduler, dwell rules, urgent queue
    agent_reel_store/     sqlite store, ring buffer, retention, raw-event policy
    agent_reel_server/    axum routes, SSE, auth, embedded UI assets
    agent_reel_views/     read models consumed by /api and UI
    agent_reel_security/  display tokens, LAN mode, CORS, mutation safety
    agent_reel_metrics/   counters, health, adapter lag, dropped events
    agent_reel_install/   doctor, discovery, shims, hook merge/uninstall
    agent_reel_identity/  provider-neutral identity model
    agent_reel_identity_github/ github login/id/profile resolver
    agent_reel_directory/ route parser, signed feed directory, tickets
    agent_reel_p2p_proto/ signed feed profiles, story capsules, grants
    agent_reel_p2p/       native publish/subscribe runtime boundary
    agent_reel_auth/      provider-neutral identity model
    agent_reel_auth_github/ github profile and device-flow boundary
    agent_reel_social/    feed directory and follow request state
    agent_reel_browser/   wasm/browser peer bootstrap boundary
    agent_reel_p2p_browser/ browser route states and discovery view models
    agent_reel_edge/      bootstrap, auth, directory, browser seed edge
    agent_reel_deploy/    deployment templates and operator surfaces
    agent_reel_testkit/   fake streams, fixtures, projector snapshots
    agent_reel_ui/        self-contained HTML/CSS/TS broadcast client
```

## roadmap

m0 - signal path:

* local server
* embedded `/reel` page
* generic ingest endpoint
* canonical event model
* SSE stream
* one-card stage UI

m1 - codex / claude:

* codex `exec --json` adapter
* claude `stream-json` adapter
* basic hook helper
* redaction
* highlight scoring
* sqlite store

m2 - auto-init:

* doctor
* config discovery
* hook merge with backups
* shell shim install
* systemd or launchd service
* uninstall restore

m3 - broadcast UI:

* stage layout
* breaking layout
* ticker
* idle state
* recaps
* wall mode
* ambient mode
* projector snapshot tests

m4 - production safety:

* aggressive redaction suite
* display/admin token split
* raw quarantine
* retention policy
* adapter backpressure
* reconnect snapshots

m5 - story compiler:

* story windows
* settle conditions
* context scoring
* anti-spam policy
* p2p-safe capsule model
* publisher-side feed summaries
* modular guardrails and external summarizer processors

m6 - native p2p alpha:

* agent_reel_p2p_proto
* native p2p node boundary
* feed profiles
* public feed capsules
* snapshot request-response

m6.1 - username route model:

* remote user route parser
* privacy-preserving query compiler
* waiting-state browser view model

m6.2 - github identity resolver:

* github login to durable id model
* edge-mediated cli/browser github sign-in
* profile view and resolver trait
* signed discovery ticket
* fake github tests

m6.3 - directory records:

* feed directory entries
* stream descriptors
* signed browser seeds
* edge resolver API shape

m6.4 - native p2p discovery:

* github user directory announcements
* opaque github user topics
* provider-key fallback surface
* signed feed record discovery tests

m7 - protocol depth:

* codex app-server adapter
* mcp stream observer
* claude otel ingest
* adapter health cards
* schema generation

m8 - distribution:

* signed binaries
* homebrew tap
* nix flake
* cargo install path
* release checklist
* operator runbook

## development

```sh
cargo xtask check
```

`just check` is a thin wrapper over the same command.
