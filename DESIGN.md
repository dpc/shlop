# tau Design Notes

Living document of architectural decisions and design direction.

Pre-implementation design exploration lives in `doc/ORIGINAL_DESIGN.md`.

## Core threading model

Prefer a single thread blocking on one `mpsc::Receiver`, reacting to
events.  Each connection has two threads — a reader and a writer.

```
  reader thread ──┐                               ┌── writer thread ──► stdin/socket
  reader thread ──┤                               ├── writer thread ──► stdin/socket
  reader thread ──┼──► mpsc::Receiver ──► event ──┤
  accept thread ──┘         loop        dispatch  └── writer thread ──► stdin/socket
                                                        (channel per connection)
```

**Reader threads** decode incoming events from each connection's
stream and feed them into one shared channel.  The event loop wakes
instantly on any event — no polling, no timeouts, no `select!`.

**Writer threads** own the write half of each connection.  The bus
delivers outgoing events by sending to the writer's per-connection
channel (`mpsc::Sender<Event>`).  This is non-blocking — a slow or
blocked consumer never stalls the event loop.  The writer thread
drains its channel and writes to the stream.

When a connection is disconnected from the bus, the `ChannelSink`
(which holds the `Sender`) is dropped.  This closes the writer's
channel, which triggers the writer thread's shutdown sequence:

- For supervised children: send `LifecycleDisconnect`, close stdin
  (drop the writer), wait for exit with timeout, then signal escalation
  (SIGTERM → SIGKILL).  The writer thread owns the `Child` handle.
- For socket clients and in-process peers: just drop the writer, which
  closes the stream.

This means `bus.disconnect(id)` is the single operation that tears
down a connection — the writer thread handles the rest autonomously.

**Design invariants:**

- Instant wakeup on any event from any source
- No wasted cycles on idle sources
- Linear scaling with extension count (adding sources = cloning the
  shared Sender)
- One thread owns all mutable state (the event loop)
- Slow writers never block the event loop
- Shutdown is naturally coordinated through channel closure

Timeouts are acceptable only as safety nets (e.g. extension startup),
never as the primary dispatch mechanism.

## Process topology

```
  tau chat ──(unix socket)──► tau component harness ──(stdio)──► extensions
  (UI client)                  (daemon process)                  (agent, fs, shell)
```

- `tau chat` always connects to a harness daemon as a socket client.
  By default it starts a fresh daemon; future `tau chat -r` will
  reattach to an existing one.
- The harness daemon manages extensions, the event bus, tool routing,
  and session persistence.
- Extensions are child processes speaking CBOR over stdio.
- The chat's TUI renders events received from the harness via a
  background reader thread and the thread-safe `TermHandle`.

## Daemon runtime directory

Adapted from selfci's mq daemon. Each harness instance gets a
directory under `$XDG_RUNTIME_DIR/tau/{pid}/` containing:

- `tau.sock` — Unix socket for client connections
- `tau.dir` — project root path (discovery marker)
- `tau.pid` — daemon process PID

The marker file `tau.dir` is written **after** extensions are started
and the socket is bound. Finding it guarantees the harness is ready.

## Event protocol

CBOR-encoded events over self-delimiting streams (stdio pipes or Unix
sockets). Same protocol regardless of transport. Event families:

- `lifecycle.*` — hello, subscribe, ready, disconnect
- `tool.*` — register, unregister, request, invoke, result, error,
  progress, cancel, cancelled
- `message.*` — user, agent
- `extension.*` — starting, ready, exited, restarting

The bus routes events to subscribed connections. Tool requests are
resolved to a live provider and forwarded as directed invocations.

## Extension model

Extensions are standalone executables. Each extension:
- starts as an ordinary process
- handshakes (hello → subscribe → register tools → ready)
- sends and receives events over stdin/stdout
- is automatically unregistered when it disconnects

Tool ownership is explicit — the harness tracks which connection
provides each tool and routes requests accordingly.

## Single binary

Tau ships as one binary containing CLI, harness, agent, and built-in
tools. The hidden `tau component <name>` subcommand dispatches to
extension entry points. See `ARCHITECTURE.md` for details.
