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
`<state_dir>/<session_id>/events.cbor` (length-prefixed CBOR stream). The
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

### Reasoning effort

Tau exposes a uniform six-level effort knob — `off`, `minimal`, `low`,
`medium`, `high`, `xhigh` — that maps onto provider-specific reasoning
controls. Defaults can be set per-model:

```json5
// harness.json5
default_efforts: {
  "anthropic/claude-opus-4-7": "high",
},
```

In the UI: `/effort medium`, or hit `Shift+Tab` to cycle.

### Prompt input caching

For providers that support it, Tau emits stable `prompt_cache_key` routing
keys (derived from base URL, model id, and session cwd) so cache hits survive
restarts and parallel sessions, and sets `prompt_cache_retention` where
available. Provider compatibility flags live next to the model entry
(`supports_prompt_cache_key`, `supports_prompt_cache_retention`). Toggle the
status-bar hit-rate readout with `/show-cache-stats`.

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
    },
  },
},
```

### `core-agent` — LLM backend

The conversation driver: assembles prompts, streams provider responses, drives
tool invocations, emits reasoning blocks, and respects the effort knob. Talks
to OpenAI-compatible Responses-API and Chat-Completions-API providers; manage
credentials with `tau provider add` / `tau provider login`.

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
| `/tree [id]`        | Print session tree; with `id`, rewind head           |
| `/show-diff`        | Toggle expanded diffs vs. compact `+N/-M` chip       |
| `/show-thinking`    | Toggle agent reasoning summaries                     |
| `/show-cache-stats` | Toggle prompt-cache hit stats in the status bar      |

Toggle state is persisted to `<state_dir>/cli.json`.

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
final answer, styled distinctly from the response. `/show-thinking` toggles
visibility globally; past blocks re-render in place when the toggle flips, so
you can hide them after the fact. Reasoning blocks are not replayed back to
the provider as input — they remain provider-side context.

### Diff rendering

File mutations made by `write` and `edit` render as inline diffs. By default
they collapse to a compact `+N/-M` chip; `/show-diff` expands them to the
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
