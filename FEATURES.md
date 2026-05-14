# Features

A guide to the major features of (dpc's) Tau coding agent. For high-level
philosophy and motivation see [README.md](README.md); for design notes see
[DESIGN.md](DESIGN.md) and [ARCHITECTURE.md](ARCHITECTURE.md).


## Architecture

### Process-oriented components

Every major component — UI, harness, LLM provider, extensions — runs as a
standalone POSIX process and talks CBOR-encoded events over stdio (extensions)
or a Unix socket (UI ↔ harness). A component is just an executable: supervise
it with your init system, sandbox it with bubblewrap or Landlock, swap it for
anything else that speaks the protocol, or write a new one in any language.

The default `tau` binary bundles all first-party components and dispatches via
hidden `tau ext <name>` subcommands; you can replace any of them by editing
`harness.json5`.

### Persisted event log

Every protocol event in a session is appended to
`<state_dir>/sessions/<session_id>/events.cbor` (length-prefixed CBOR stream). The
in-memory [`SessionTree`] is rebuilt from the log on resume, so the on-disk
record and the live view cannot drift. Because the log is a stream of typed
events rather than a flat transcript, sessions branch into a tree: rewinding
to an earlier turn keeps the abandoned branch on disk.

```
$ tau session-list
$ tau session-show --session-id <id>
$ tau -r                  # pick a recent session for this cwd
$ tau -r <id>             # resume a specific one
```

Inside the UI, `/tree` prints the branch graph and `/tree <node-id>` rewinds
the head to that node.

### Interception system

Components can register as event interceptors with priority + selector pairs;
matching events are routed through the interceptor and only reach the bus and
event log if it allows them. Exact selectors win over prefix matches; ties are
broken by component name. This is how things like the policy gate or the
delegate progress tracker plug in without modifying the harness core.

See [`docs/interceptors.md`](docs/interceptors.md) and
`crates/tau-harness/src/interception.rs`.

### Remote extensions over SSH

Because extensions are stdio child processes, running one on another machine
is a matter of prefixing its argv with an `ssh` invocation:

```json5
// harness.json5
extensions: {
  "core-shell": {
    prefix: ["ssh", "user@host"],
  },
},
```

The harness prepends `prefix` to the resolved command. Anything that gives you
a stdio pipe to a remote process works the same way (`docker exec`, `nsenter`,
`bwrap`, …).

### Model parameters: effort, verbosity, thinking summary, service tier

Per-prompt knobs are bundled into a single `ModelParams` struct
that the harness stamps onto every `SessionPromptCreated` and that
backends thread through to the provider request:

- **`effort`** — reasoning effort. Six levels (`off`, `minimal`, `low`,
  `medium`, `high`, `xhigh`); maps to OpenAI `reasoning.effort` /
  `reasoning_effort`. The `xhigh` rung is gated per model: a curated
  whitelist covers known OpenAI models (`gpt-5.5`, `gpt-5.4`/
  `gpt-5.4-pro`, `gpt-5.3-codex`, `gpt-5.2`, `gpt-5.1-codex-max`,
  excluding `mini`/`nano` variants), and individual model entries can
  opt in or out explicitly with `supportsXhigh: true|false`. For
  asymmetric models like `gpt-5.4-pro` (medium/high/xhigh only — no
  `off`/`low`) a `reasoningEfforts` list on the model entry pins the
  exact accepted levels.
- **`verbosity`** — output verbosity (`low`, `medium`, `high`).
  Sent to providers that advertise `supportsVerbosity` as
  top-level `verbosity` (Chat Completions) or `text.verbosity`
  (Responses). Default `low` to keep model replies concise. Per-model
  `supportsVerbosity` and `verbosities` overrides mirror the effort
  escape hatches.
- **`thinking_summary`** — reasoning-summary mode (`off`, `auto`,
  `concise`, `detailed`). Sent as `reasoning.summary` on providers
  that set `supportsReasoningSummary`; ignored otherwise.
- **`service_tier`** — optional upstream service tier. `/fast`
  toggles Codex's `fast` tier. Backends serialize Codex's exact
  OpenAI wire values: `priority` for Fast and `flex` for Flex.

Per-model escape hatches in `models.json5`:

```json5
providers: {
  openai: {
    compat: { supportsVerbosity: true },
    models: [
      { id: "gpt-5.4-pro", reasoningEfforts: ["medium", "high", "xhigh"] },
      { id: "gpt-5-locked", supportsVerbosity: false },
    ],
  },
},
```

Defaults are normally selected through agent roles in `models.json5`:

```json5
defaultRoles: {
  default: { model: "openai/gpt-5.5", effort: "medium" },
  smart: {},
  deep: { effort: "xhigh", thinkingSummary: "detailed" },
  rush: { effort: "low", fastMode: true },
},
```

`/model <role>` switches roles; `/role <role> <setting> <value>` edits or
creates a persisted role override. See [`docs/agent-roles.md`](docs/agent-roles.md).

In the UI: `/effort medium`, `/verbosity low`, `/thinking-summary
concise`. Shift+Tab cycles **effort** specifically; the other knobs
are slash-command-only today. The cycle walks levels the harness
reports as available for the current model, so `xhigh` is reachable
on `gpt-5.5` and skipped on `gpt-5.4-mini`. Asking for an unsupported
level (e.g. `/effort xhigh` on a mini model, `/verbosity high` on a
provider that doesn't support it) degrades and surfaces a
`HarnessInfo` notice rather than silently dropping the field.

The status bar renders effort always; Fast mode, non-default verbosity,
and thinking-summary are appended as `, fast` / `, v=<level>` /
`, ts=<level>` so a fresh session stays compact and only grows when the
user changes a knob.

### Prompt input caching

For providers that support it, Tau emits stable `prompt_cache_key` routing
keys (derived from base URL and session id) so cache hits survive restarts
within a session, and sets `prompt_cache_retention` where
available. Extension-originated turns (e.g. `core-delegate` sub-agents) get a
distinct key so parallel delegations don't pile onto the user agent's routing
bucket and trip OpenAI's `>15 RPM`-per-key overflow heuristic. Provider
compatibility flags live next to the model entry (`supports_prompt_cache_key`,
`supports_prompt_cache_retention`). Toggle the status-bar hit-rate readout with
`/set show-cache-stats <true|false>`.

### Policy / approvals

Subscription approvals are persisted to `<state_dir>/policy.cbor` so that
trusted client/selector pairs don't re-prompt on every reconnect. View them
with `tau policy-show`.


## Built-in extensions

Every built-in extension is a regular extension under
`crates/tau-ext-*/`. Each is configured under `extensions.<name>` in
`harness.json5` and can be disabled with `enable: false`, swapped via
`command:` / `prefix:`, or given free-form `config:` payload that arrives at
startup as a `LifecycleConfigure` message.

```json5
extensions: {
  "core-shell":         { enable: false },               // disable
  "core-agent":         { prefix: ["ssh", "user@host"] },// run remotely
  "std-notifications": { config: { idle_seconds: 30 } },// reconfigure
},
```

### `core-shell` — shell and filesystem tools

Registers the everyday tools the agent uses to inspect and edit a project:
`shell`, `read`, `write`, `edit`, `grep`, `find`, `ls`, plus an `echo` tool
for testing. The shell command and any wrapper prefix are configurable:

```json5
"core-shell": {
  config: {
    shell: {
      command: "bash",
      prefix: ["nix", "develop", "-c"],
      // User-initiated `!`/`!!` commands are killed after this many
      // seconds. Tool-invoked `shell` calls use their own per-call
      // `timeout` argument (default 120s). Default: 3600 (1 hour).
      user_command_timeout_secs: 3600,
      // Extra env vars injected into `shell` and `!`/`!!` children,
      // applied after the inherited environment so they override or
      // supplement it. Use this to set a custom `PAGER` or adjust paths.
      extra_env: {
        XDG_CONFIG_HOME: "/home/me/.config",
        PAGER: "cat",
      },
    },
  },
},
```

### `core-agent` — LLM backend

The conversation driver: assembles prompts, streams provider responses, drives
tool invocations, emits reasoning blocks, and respects the effort knob. Talks
to OpenAI-compatible Responses-API and Chat-Completions-API providers; manage
credentials with `tau provider add` / `tau provider login`.

On Responses-API backends (OpenAI Codex / ChatGPT subscription), conversations
chain via `previous_response_id` after the first turn: each follow-up request
sends only the messages added since the prior `response.id` and lets the
upstream API carry reasoning state forward server-side. The chain is dropped
automatically on model switches, branch edits (`UiNavigateTree`), and turns
that didn't return a `response_id`; if the upstream rejects the stored id
(server-side expiry), the agent falls back to a full-replay retry once before
surfacing the error.

The Codex backend additionally routes turns over a persistent **WebSocket**
connection (auto-enabled for `chatgpt.com/backend-api`; toggle for custom
endpoints via the `supportsWebsocket` provider compat flag). One connection
per `(account, session)` lives in a small LRU pool inside the agent process,
so the server-side connection-local response cache stays warm across turns of
the same conversation — including when the agent context-switches between
sessions (sub-agent delegations interleaved with the parent). Connections
age out before the upstream's 60-minute hard cap, and refreshed OAuth tokens
invalidate stale sockets on next use. Per-session sticky fallback to HTTP+SSE
kicks in if the WS upgrade gets `426 Upgrade Required` or the server signals
`websocket_connection_limit_reached`, so a misbehaving upstream never breaks
a prompt outright.

### `std-notifications` — idle and turn notifications

Plays a sound on prompt submit and on the final response of a turn. After
`idle_seconds` of inactivity following a final response (default 60s) it asks
the agent for a one-sentence summary and emits a desktop notification — useful
when a long task finishes while you're in another window.

### `core-delegate` — sub-task delegation

Exposes a `delegate` tool that spawns a side conversation, runs it to
completion against the same model and tool set, and returns its result to the
caller. The sub-agent starts with a *fresh* context — only the parent's
`prompt`, the system prompt, and tools — with no visibility into the parent
conversation's prior turns, tool results, or in-flight state. The same
isolation applies at every nesting depth, so sub-sub-agents don't see
ancestor task framing and can't be tricked into re-delegating it. Parent
agents are responsible for putting everything the sub-agent needs into the
`prompt`. Live progress (turns, current tool) is shown in the parent UI
alongside the delegate's task name.

### `std-websearch-exa` — opt-in web search

Proxies a single `websearch_exa` tool to Exa's hosted `web_search_exa` MCP
endpoint. Disable in `harness.json5` when not needed; supply an API key via
config.


## CLI / UI

Tau ships a terminal UI that aims for *every pixel of estate is content* —
fast startup, no chrome.

### Slash commands

Type `/` for menu autocompletion. The built-in set:

| Command             | Effect                                               |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the session                                     |
| `/detach`           | Leave the UI, keep the harness running for reattach  |
| `/new`              | Start a fresh session in this harness                |
| `/model <id>`       | Switch model (Tab completes from provider list)      |
| `/effort <level>`   | Set reasoning effort (`Shift+Tab` cycles)            |
| `/verbosity <level>`| Set output verbosity (`low`/`medium`/`high`)         |
| `/fast` | Toggle Codex Fast mode (`service_tier: fast`)      |
| `/thinking-summary <mode>` | Set reasoning-summary mode (`off`/`auto`/`concise`/`detailed`) |
| `/tree [id]`        | Print session tree; with `id`, rewind head           |
| `/set <name> <val>` | Set a UI setting (Tab cycles names + values)         |

Available `/set` names: `show-diff` (expanded vs. compact diffs),
`show-thinking` (agent reasoning summaries), `show-cache-stats`
(prompt-cache hit stats in status bar), `show-token-stats` (per-turn
token usage below responses). All take `true` / `false`. The first-arg
completion menu shows each setting's current value; the second-arg
menu shows the meaning of each allowed value. State is persisted to
`<state_dir>/cli.json`.

### Path autocompletion

When the prompt buffer starts with `./` or `../`, Tab triggers filesystem path
completion against the current working directory — handy for naming files in
free-form prompts. Standard fzf-style fuzzy-search bindings are also available
inside the completion menu. Slash-command arguments use the same menu but are
populated dynamically by the harness (model list, effort levels, …).

### Bang shell commands

A prompt line starting with `!` runs a shell command from the UI. `!<cmd>`
renders live stdout/stderr in the transcript and injects the finished output
back into the session context as a `<user_shell>` block, so the agent can see
what you ran and use the result.

Use `!!<cmd>` for UI-only commands: output is rendered the same way, but is
marked `[no context]` and is not replayed to the agent.

Examples:

```text
!ls
!!git status
```

### Customizable key bindings

`cli.json5` exposes a `bind:` table that maps key chords to prompt-local
shell actions. Bindings are layered on top of built-ins; user entries with the
same key replace the built-in binding.

Supported actions:

- `shell-prompt-edit`: dump the prompt to `$TAU_PROMPT_PATH`, run the shell
  command, then replace the prompt with the file contents on success.
- `shell-prompt-insert`: run the shell command and insert its stdout at the
  cursor on success.
- `fast-toggle`: toggle Fast mode directly. For example:
  `{ action: "fast-toggle" }`.
- `role-cycle`: cycle to the next available agent role directly. For example:
  `{ action: "role-cycle" }`.

Command environment:

- `TAU_PROMPT_PATH`: tempfile containing the current prompt.
- `TAU_PROMPT_ROW` / `TAU_PROMPT_COLUMN`: 1-indexed cursor position for
  editor commands that support `file:row:column` syntax. Multi-line row
  calculation is still limited.

Default bindings:

```json5
bind: {
  "C-f": {
    action: "shell-prompt-insert",
    command: "rg --files --hidden --glob '!.git' | fzf --height=100%",
    trim: true,
  },
  "C-s": { action: "role-cycle" },
  "C-r": {
    action: "shell-prompt-insert",
    command: "RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind \"change:reload:$RG_PREFIX {q} || true\" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\\''line - 4 <= NR && NR <= line + 4 { printf \"%6d  %s\\n\", NR, $0 }'\\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1",
    trim: true,
  },
  "C-o": {
    action: "shell-prompt-edit",
    command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"",
  },
  "C-g": {
    action: "shell-prompt-edit",
    command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"",
  },
},
```

A Helix override:

```json5
bind: {
  "C-o": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
  "C-g": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
},
```

Use `trim: true` for commands like `fzf` whose selected value ends with a
newline you do not want inserted into the prompt.

### `Ctrl+O` — edit prompt in your editor

The default `C-o` binding suspends the UI, opens the prompt in `$EDITOR`, and
replaces the buffer with whatever you save. Redraws are paused while the
editor owns the terminal.

The editor file also includes a Markdown trailer after:

```md
<!-- TAU trailer: everything after this line will be ignored -->
```

Everything after the marker is ignored when Tau reads the file back. The
trailer quotes useful context for composing the next prompt: the current
in-flight response, the last agent response, and the previous submitted prompt.
Leading and trailing blank lines around the editable prompt are trimmed.

### `Ctrl+F` — fzf (or anything else) into the prompt

Because bindings are arbitrary shell commands, wiring fzf or another picker
into the prompt is straightforward:

```json5
bind: {
  "C-f": {
    action: "shell-prompt-insert",
    command: "rg --files --hidden --glob '!.git' | fzf --height=100%",
    trim: true,
  },
  "C-r": {
    action: "shell-prompt-insert",
    command: "RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind \"change:reload:$RG_PREFIX {q} || true\" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\\''line - 4 <= NR && NR <= line + 4 { printf \"%6d  %s\\n\", NR, $0 }'\\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1",
    trim: true,
  },
},
```

`C-r` starts with an empty result list; type a query to search file contents
with `rg`, preview the matching context, and insert the selected file path.
Replace `rg --files | fzf` or the content-search command with `git ls-files`,
a custom script, or whatever fits your workflow.

### Thinking / reasoning rendering

When the model emits reasoning blocks, the UI renders them inline above the
final answer, styled distinctly from the response. `/set show-thinking <true|false>`
toggles visibility globally; past blocks re-render in place when the flag flips, so
you can hide them after the fact. Reasoning blocks are not replayed back to
the provider as input — they remain provider-side context.

### Diff rendering

File mutations made by `write` and `edit` render as inline diffs. By default
they collapse to a compact `+N/-M` chip; `/set show-diff true` expands them to the
full unified hunk view. The terminal renderer uses cell-level differential
updates to avoid full repaints on each token.

### Theming

The UI ships with a built-in Solarized-derived "tau" theme. Themes map
semantic style names (`prompt.marker`, `banner.accent`, `system.info`, diff
hunks, reasoning blocks, …) to terminal attributes; user themes can be
loaded from a JSON5 file. See `crates/tau-themes/themes/tau.json5` for the
full style key list.

### Session resume and detach

`/detach` leaves the harness daemon running so the agent can keep working in
the background; `tau --attach` reconnects later. `tau -r` opens a picker for
recent sessions in the current `cwd` (showing lock status and the latest user
prompt), `tau -r <id>` picks a specific one. The session tree, including
abandoned branches, is preserved across restarts.
