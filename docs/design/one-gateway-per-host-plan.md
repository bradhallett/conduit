# Implementation plan: one heavy gateway per host

Companion to [`one-gateway-per-host.md`](one-gateway-per-host.md). That document is the
design; this one is the build order. It is a multi-PR architecture change, not a single
gateway edit, and it should be read as a checklist rather than a commitment to land it in
one release.

## Current state

Grounded in the code as of 2026-09-16 (through #894). Anything marked "not started" below
is the honest next work, not a claim about ordering.

Landed:

- Phase 0. `src-tauri/src/topology.rs` defines `GatewayRole`, `CompatKey`, `LaunchKey`,
  `TopologySnapshot`, and the topology assertions.
- P2.1 rendezvous primitives (`src-tauri/src/daemon.rs`, #880), plus the daemon roles:
  P2.2a identity and P2.2b the full host runtime on the internal endpoint (#881), with a
  cold-start test (#882).
- P2.2c the stdio adapter, `--stdio-adapter` (#888), with a bounded worker pool for
  concurrent requests (#891) and recovery after the daemon dies (#893). Opt-in only; the
  default stdio role is untouched.
- P2.3 daemon idle exit (#892) and adapter crash recovery (#893). The lease is the open
  connection: the daemon exits once nothing has been in flight for `DAEMON_IDLE_GRACE`,
  and the adapter re-rendezvouses and replays the client handshake on the next request
  after a failure, without replaying the call that failed. Session TTL reaping
  (`reap_stale_mcp_sessions`) predates this work.
- P1.2 first increment: `session_store::SessionStore` (#894), a TTL and cap store for the
  session-scoped maps, as a tested module with nothing wired to it yet.
- P1.2 maps and owner: the PII pseudonym map, shaped-result cursors, and modern HITL
  approvals moved off their ad-hoc process globals onto `SessionStore`, and a
  `SessionTables` owner now holds the PII and HITL tables (the shaped stash is owned by
  `shaping`). Behavior is unchanged; the tables are now TTL- and cap-bounded, with
  reap-on-close and cap tests.
- P1.2 transport unification: `McpSession` (HTTP) and `StdioUpstream` (stdio) are now one
  `SessionState` with a transport face, and `StdioUpstream` is deleted. The upstream
  call/correlation logic and the notification fanout have a single implementation; the
  stdio face writes straight to stdout, the HTTP face queues for the listen stream.
- P1.2 threading, first increment: the stdio client's declared capabilities and its
  `${ROOT}` project root moved from `GatewayState` onto the stdio `SessionState`, and the
  server-request handler and roots refresh read them there.
- P1.2 threading, second increment: `MODERN_STDIO_UPSTREAM` moved off the process onto
  the stdio `SessionState`, and the notification and resource-updated paths now carry that
  session instead of a bare stdout, so the sink a frame is written to and the era that
  decides whether it may be written come from one owner. The stdio client's progress
  hand-off queue became session state (the shared progress dispatch now closes over the
  stdio session instead of capturing a process stdout), and the search and confirm guards
  moved onto `SessionState`, with the listener-level pair kept only for requests that carry
  no session record (a modern request, or an OpenAPI call). The `GatewayState.stdout` field
  is gone with them. Progress routes stay host-scoped by decision, see below.

Still open:

- P1.2 threading, remainder: the `STDIO_*` handshake statics (`STDIO_CLIENT_READY`,
  `STDIO_RESPONDED`, `STDIO_DEFERRED_LIST_CHANGED`) still encode one stdio client. They
  move with the stdio connection object, after the deferred queue is keyed per session.
  Three more single-stdio assumptions go with that move: the reader's `CancelRegistry` and
  in-flight cap are per-process and keyed by client-chosen JSON-RPC ids, so two stdio
  connections could cancel each other; `write_stdio_response`'s no-face branch flips the
  process-wide `stdout_broken` for what is a per-session condition; and stdio PII/HITL
  lookups collapse to `PII_LOCAL_SESSION`, so two stdio clients on one host would share one
  pseudonym map and clearing one would clear the other.
- P1.3 `HostState` (in progress). The host runtime now lives on `HostState` (registry and
  its trust flag, router, catalog snapshot, routine candidates and advisor, ready/dirty
  flags, rebuild lock, listener config, server handler, resource subscriptions and the
  `resources/updated` sink), together with its session table, its daemon runtime (daemon
  flag and activity lease), its rebuild streak map, its quarantine read flag, and the
  progress token counter. What remains outside is `DISCOVERY_MODE`, `CODE_MODE`, the
  principal-keyed `session_tables()` store, and the `PROGRESS_*` dispatch and routes, which
  are read inside the dispatch core (see the P1.3 section for why those need the core's
  signatures changed rather than a field move).
  `GatewayState.stdio_upstream` is also constructed unconditionally, including in
  HTTP/daemon mode where there is no connection.
- Discovery and code mode are host policy, not session state, by decision. Both are
  resolved from the registry (which the watcher refreshes live) plus a process env
  override, so every session on one host sees the same switch; the per-client part of
  discovery already resolves per request from the caller's client id, and a daemon session
  will resolve it from the identity asserted at session open. Moving them onto
  `SessionState` would give each session a private copy of a host-wide setting.
- Progress routing is host state by decision: one token table per host, with every entry
  recording the session key that minted its token (a real session id for an HTTP client,
  the `RESOURCE_SUB_STDIO` sentinel for the stdio client). The stdio half of it is still
  single-client: the dispatch closes over the gateway's stdio session and that sentinel is
  a constant, so a second stdio client needs its own route identity. What was
  session-shaped about it (the hand-off queue and the stdout it writes to) is now owned by
  the stdio session.
- Progress is not era-gated, and never was: `deliver_progress` writes a bare
  `notifications/progress` frame through the hand-off, without the `list_changed` /
  `resources/updated` check on the peer's declared era. Pre-existing, unchanged by the
  threading work, and on the list so it is not read as an oversight.
- No topology feature flag in the registry.
- Two tests resolved the data directory per call on paths `DataDirOverride` was not
  guarding, so the gateway suite wrote into the developer's real data dir: the audit writer
  (`audit::audit_path`) and the search-trace writer (`searchtrace::path`). A full run
  appended 41 audit rows and 25 search-trace rows; the audit half also let one test's
  fixture row land inside another test's scratch log, which is what failed
  `mcp_http_audit_entry_records_client_and_client_name` intermittently on CI. Fixed by a
  test-only `DataDirTestEnv` guard (ENV_LOCK plus a scratch override) on every test that
  can reach either writer. Any future per-call `conduit_dir()` resolution needs the same
  treatment, or the leak returns under a third name.
- Unrelated and still open: several tests leak their own scratch directories under the temp
  dir, because a panicking test skips its cleanup and a failing run leaves the directory
  behind. A long local session accumulated about 1,900 of them (`toolport-pii-release-*` was
  the largest group). Worth one small cleanup pass with a Drop guard on those specific
  tests; it does not affect correctness, but it makes the temp dir useless as a signal.
- The adapter has not been dogfooded against a real client (P4.1), but it has an early
  synthetic signal: with one stdio downstream (9 tools) and three client sessions, the
  legacy arm ran 3 gateways and 3 downstream copies while the `--daemon` + `--stdio-adapter`
  arm ran 1 daemon, 3 thin adapters, and 1 downstream copy, with all three sessions
  answering a real `tools/call` on both arms. That is the pooling factor moving from 3.0 to
  1.0. It is a small local fixture, not the acceptance run, and it says nothing about
  cold-start or first-call latency, so the Phase 0 baseline numbers still stand as the
  real-machine measurement.
- Reusable primitives that already exist: the approval broker's `EndpointDescriptor`
  (`approval.rs`), `registry::atomic_write`, and the registry cross-process `FileLock`.
- Client launch: `clients.rs::gateway_entry` builds the stdio entry and sets
  `TOOLPORT_CLIENT_ID`. The desktop app spawns `--http <port>` through
  `http_bridge.rs::start_with_token_at` and kills it on exit.

## Delivery shape

| PR  | Slice                                                                                                              | Behavior change                             | Status                                                             |
| --- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------- | ------------------------------------------------------------------ |
| 1   | P2.1 rendezvous primitives (library module, tested)                                                                | none (new module only)                      | landed (#880)                                                      |
| 2   | P2.2a identity role; P2.2b host runtime on the internal endpoint                                                   | none (explicit flag only)                   | landed (#881)                                                      |
| 3   | P1.2 session tables on `SessionStore`; transports unified on `SessionState`; era, progress, and guards per session | none default; HTTP confirm scoping narrowed | landed; handshake statics remain                                   |
| 4   | P1.3 `HostState` extracted; `GatewayState` becomes a thin facade                                                   | none                                        | in progress: three increments landed, dispatch-core statics remain |
| 5   | P2.2c stdio adapter speaks the daemon session protocol, behind flag                                                | opt-in only                                 | landed (#888, #891, #893)                                          |
| 6   | P2.3 session lifecycle, TTL, crash/EOF handling, fallback                                                          | opt-in only                                 | landed (#892, #893)                                                |
| 7   | P3.1 union catalog built once, allowed-set enforced per session                                                    | opt-in only                                 | not started                                                        |
| 8   | P3.2 downstream pooling by `LaunchKey` and `${ROOT}` sharding                                                      | opt-in only, the big win                    | not started                                                        |
| 9   | P4.1 dogfood flag, telemetry, acceptance run                                                                       | opt-in only                                 | not started                                                        |
| 10  | P4.2 adapter topology becomes default; legacy kill switch remains                                                  | default flip                                | not started                                                        |
| 11  | P4.3 desktop Shared HTTP converges onto a daemon service lease                                                     | separate, later                             | not started                                                        |

Each of 1 through 8 must leave the default topology untouched and all existing suites
green. The only PRs that change what a user gets are 10 and 11.

## Phase 1: explicit HostState, SessionState, RequestContext

Split `GatewayState` along the boundary the design already names, without changing
behavior. This is the prerequisite for sharing a runtime safely.

### P1.1 RequestContext

Largely satisfied already by SBS-551, so there is no separate PR for it. The
per-request fields (`upstream_version`, `upstream_capabilities`, `mcp_session`,
`upstream_transport`) live in one `ActiveRequestContext`, are installed at the dispatch
boundary (`handle_request_with_cancel` enters the era and capabilities guards;
`process_request` enters the transport; the HTTP path enters the session), and every
read goes through the context (`serving_modern_client`, `active_mcp_session`,
`active_upstream_is_stdio`, `modern_client_supports_server_rpc`). The design permits the
thread-local as a scoped adapter as long as it is populated from the explicit value and
cannot outlive the request, which the guards already ensure.

What remains is not per-request but session- and host-scoped: the `STDIO_*` handshake
flags and `PROGRESS_*`. Those are single-stdio-client assumptions and move in P1.2
(`SessionState`) and P1.3 (`HostState`), so the isolation work lands with the types that own
it instead of as a mechanical rewrite of the request path. The PII and HITL tables already
moved onto a `SessionStore` owner, the two transport types are unified, and the stdio
client's protocol era, its progress hand-off queue, and the search and confirm guards are
session state; the handshake statics are the last of the session-scoped state.

### P1.2 SessionState

Status: store landed (#894), the three session-scoped maps moved onto it, the two
transport types are unified, and the stdio session now owns the client's capabilities and
root, its declared protocol era, its progress hand-off, and its search and confirm guards.
The remaining globals are the `STDIO_*` handshake statics plus the host policy and routing
that P1.3 takes.

- Introduce `SessionState` owning exactly what the design lists: session id, principal and
  audit label, effective scope, protocol version and capabilities, roots and `${ROOT}`,
  upstream request correlation, outbound queue, cancellation, subscriptions, search guard,
  confirm guard, connection-local notification eligibility. Today it owns the transport
  face, owner, upstream request correlation, outbound queue, subscriptions, root, the
  stdio client's declared capabilities, its 2026-07-28 era flag, its progress hand-off
  queue, and its guard pair; the `STDIO_*` handshake statics move with the stdio connection
  object, and discovery/code mode plus the progress routes are host-scoped by decision.
- `McpSession` becomes the HTTP transport face of `SessionState`; the stdio path gets the
  same type with a stdio transport face, deleting `StdioUpstream` as a separate concept.
  Landed: one `SessionState` with `SessionTransportFace::{Http, Stdio}`. The upstream
  call/correlation and the list_changed and resources/updated fanout each have a single
  implementation, and the notification path reads the sink and the era off the session.
- PII maps, shaped-result cursors, and modern HITL approvals stay keyed by principal, but
  live behind a `SessionStore` with explicit TTL and cap. Landed: `session_store::SessionStore`
  (#894) plus the PII map, shaped-result cursors, and modern HITL approvals moved onto it,
  with reap-on-close and reap-on-TTL/cap tests, owned by `SessionTables`.
- Guards: the search-thrash streak and the pending destructive confirmations are session
  state. Landed: `SessionState` owns them for both faces, a request with an MCP session id
  on the HTTP bridge uses that session's pair, and only a request with no session record
  keeps the listener-level pair. Tests cover a second session neither inheriting the streak
  nor redeeming the token. Consequence worth knowing: a confirmation is now redeemable only
  from the session that minted it (a session closed or re-initialized inside the
  confirmation window loses its pending token) and only on the surface that minted it
  (`/mcp` with a session id vs. the OpenAPI path, which mints against the listener pair).
- Known duplication: `watch_registry`/`watch_tick` still take the stdio session and its
  `${ROOT}` separately, although the root is a field of that session since #898. Harmless
  (both come from the same startup resolution) and it collapses with the `HostState` move.

### P1.3 HostState

Status: three increments landed. `HostState` owns the host runtime the gateway already
resolved once per process, and `GatewayState` is now a facade over it: a `Deref` impl keeps
the host-scoped call sites reading `state.registry`, `state.router`, and friends, so moving
ownership did not rewrite several hundred lines.

- Landed: `HostState` holds the registry and its trust flag, the live router, the catalog
  snapshot, the routine candidate registry and advisor ledger, the ready and dirty flags,
  the rebuild lock, the listener configuration (`lazy`, `http`, bind host, allowed
  origins), the server-request handler, the resource subscription table, and the
  `resources/updated` dispatch sink. `GatewayState` keeps the session-side fields: the
  profile handle, the MCP session table, the stdio client's session, and its client id and
  boot profile. One invariant test asserts that a second facade shares the host, so one
  host still has exactly one live router and one registry.
- Landed, second increment: the host now also owns its session table, its daemon runtime,
  and the progress token counter. `mcp_sessions` moved onto `HostState` (the readers did
  not change at all, which is what the `Deref` facade buys), `daemon_mode` and
  `last_activity_ms` replaced the process statics of the same names with `touch_activity()`
  and `idle_for()` on the host, and the progress token counter moved onto `ProgressRoutes`
  so the table mints its own tokens instead of reading a process-wide sequence. The table
  itself is still reached through the process-global `PROGRESS_ROUTES` until the
  dispatch-core threading lands, so today this is a per-table counter on a still-global
  table; it is the right home either way, because a token is only ever resolved against the
  table that minted it, so uniqueness is needed within a table and nowhere else. Three
  tests pin the new ownership: a second host sees neither the first one's daemon flag nor
  its activity lease, each table starts its own token sequence, and the daemon identity
  route follows the host's own flag rather than any global.
- Landed, third increment: the host owns the rebuild streak map and the quarantine read
  flag, and carries them into the background threads. `preserve_collapsed_servers_guarded`
  became `HostState::preserve_collapsed_servers_guarded`, `effective_quarantine` and
  `reconcile_quarantine` take the flag, the two functions that reach them (`watch_tick`
  and `watch_registry`) take the host, and `persist_and_emit_with_sessions` became a host
  method called as `host.persist_and_emit_with_sessions(...)`. `main` now builds the host before it
  spawns the build thread and the registry watcher, so both carry `Arc<HostState>`; the
  four initializers they used to clone (server handler, rebuild lock, resource
  subscriptions, resources/updated sink) are read back off the host. One test drives a
  collapsed catalog through one host and asserts the streak accumulates there, that a
  second host's map stays empty, and that a store failure on one host does not mark
  another's read as failed.
- Remaining: `DISCOVERY_MODE`, `CODE_MODE`, the principal-keyed session store, and the
  `PROGRESS_*` dispatch and routes. `DISCOVERY_MODE`, `CODE_MODE`, and the session store
  are read deep inside the dispatch core (`execute_call`,
  `handle_request_with_cancel`), which deliberately takes narrow parameters rather than
  the whole state, so moving them means threading a host handle through that core. That
  threading is wider than it looks: `handle_request` is a test-only wrapper with 62 call
  sites, and `execute_call` is reached through the routine and script dispatch helpers, so
  the slice needs a deliberate decision about how the test helper gets its host.
  `PROGRESS_DISPATCH` and `PROGRESS_ROUTES` stay where they are for the same reason: the
  dispatch is read by `prepare_progress`, three layers below anything that holds the state,
  and the design publishes exactly one progress dispatch per host, so a process global is
  the host's in effect. Both are recorded here as deliberate, not overlooked; they should
  move with the dispatch-core threading if a process ever hosts two runtimes at once.
- Watch item: `mcp_sessions` moved onto `HostState` in this increment, which closes the
  trap the first increment's review flagged: a facade can no longer be constructed per
  session and end up with an empty table.
- Naming note: `codemode.rs` has its own private `HostState` for the QuickJS sandbox host.
  Unrelated to this one; the plan's name wins here because this is the type the design's
  ownership split is about.
- Introduce `HostState` owning registry and watcher, router and rebuild lock, catalog
  snapshot and cache writes, downstream pool and circuit-breaker state, quarantine and
  rate-limit bindings, audit/metrics/savings, `server_handler`, and
  `resource_updated_sink`.
- `GatewayState` shrinks to a facade holding `HostState` plus a `SessionStore`, so the
  existing 300 call sites keep compiling while P2 moves ownership.
- Tests: the topology assertions in `topology.rs` stay green; a host with one router and
  two sessions reports `router_owners == 1`.

#### Next slice: the dispatch core

Where the fourth increment starts, and the decision it has to make before writing code.

- Remaining holders, with the readers that keep them off `HostState`: `DISCOVERY_MODE`
  (`discovery_mode()` is read by `grouped_discovery`, `enabled_summary`, `watch_tick`,
  `handle_stdio_request`, and `main`); `CODE_MODE` (`code_mode_enabled()` is read by
  `gateway_capabilities`, `append_routine_tool_defs`, `grouped_tool_defs`,
  `advise_after_direct_call`, both `save_routine_*_dispatch` helpers,
  `handle_request_with_cancel`, and `http_tool_defs`); and the `session_tables()` store
  (`clear_pii_session`, the `modern_hitl_*` family, and the HTTP/SSE reader's drop path).
- The decision: `handle_request` is a wrapper whose 62 call sites are all tests, and
  `execute_call` is reached through `run_routine_dispatch`, `execute_script_dispatch`, and
  `execute_script_dispatch_with_candidate`. Threading `host: &HostState` through that chain
  is mechanical except for how the tests receive their host. Tests that assert PII or HITL
  continuity across calls (for example
  `clearing_a_pii_session_drops_the_previous_conversations_map`) need one host for the
  whole test, so the fixture-shaped answer is a host built once in the test body and passed
  to every call; a per-call `&http_state(false)` would silently reset the store between
  calls and quietly weaken exactly those tests.
- Also worth folding into that slice: `watch_tick` and `watch_registry` now take the host
  _and_ clones of its own fields (rebuild lock, server handler, resource subscriptions,
  `mcp_sessions`), so a caller could pair one host with another host's router or cache. The
  seven throwaway hosts in the watcher tests do exactly that on purpose. Production is
  consistent, and collapsing those parameters into the host removes the hazard.
- Sequencing question, for the maintainer rather than for the code: this increment and the
  stdio handshake statics are all that Phase 1 has left, and neither is a prerequisite for
  the pooling work. The three remaining P1.3 holders are host-scoped by decision rather than
  isolation gaps (host policy, one table per host, one dispatch per host), and the P1.2
  remainder only affects the stdio handshake path. P3.1 and P3.2 are therefore free to start
  first; the plan keeps the original order by preference, so that pooling is built on state
  that is already fully host-owned. That is a choice about risk, not a dependency.

## Phase 2: rendezvous and the stdio adapter

### P2.1 Rendezvous primitives (landed, #880)

New library module `src-tauri/src/daemon.rs`, no gateway wiring and no behavior change yet:

- `DaemonDescriptor` with `endpoint`, `token`, `pid`, `compat` (from `CompatKey`),
  `protocol`, `created_at_ms`. Written with `registry::atomic_write` and user-only
  permissions, beside the existing `approval-endpoint.json`.
- `descriptor_path(data_dir, compat)` keyed by the compat fingerprint, so mismatched
  versions and data dirs can never read each other's descriptor.
- `probe_identity(descriptor)` performing an authenticated `GET /host/identity` and
  returning the daemon's `DaemonIdentity`; `is_compatible_with` verifies the complete
  compatibility identity (version, data dir, protocol generation). Never trusts PID or an
  open port.
- `Rendezvous::ensure(spawn)` doing read, claim-check, probe, then election with a
  version-keyed `registry::lock_at_for`, a recheck under the lock, a single `spawn`, and a
  bounded readiness wait while the lock is held.
- `serve_identity(...)` for the daemon side: a tiny authenticated loopback listener that
  publishes the descriptor and answers identity.

Tests: descriptor path is compat-keyed; probe succeeds and rejects a wrong token; an
identity from one build is not compatible with another; the descriptor is owner-only; 8
concurrent cold starts elect exactly one daemon; a stale descriptor is replaced.

### P2.2 Host runtime and adapter

- P2.2a (landed, #881): `--daemon` is accepted and the P2.1 identity listener serves
  `/host/identity`.
- P2.2b (landed, #881): `--daemon` runs the full host runtime on an ephemeral loopback endpoint
  with a random internal bearer and publishes the descriptor. The internal
  `/host/identity` route is daemon-only, so the user-facing HTTP bridge never exposes the
  compat fingerprint or build. Still explicit-flag only, off the default startup path, and
  with the same registry, router, watcher, audit, and session tables as the HTTP bridge.
- P2.2c (landed, #888, #891, #893): `--stdio-adapter` performs `Rendezvous::ensure` and
  speaks the daemon session protocol with Toolport's Streamable HTTP/SSE: one session
  open, bidirectional JSON-RPC translation, cancellation, oversized frames, and
  server-initiated RPC correlated back to the originating session. Requests run on a
  bounded worker pool (notifications stay on the reader, so a cancellation stays ahead of
  what is queued behind it); a request that fails at the transport level is reported and
  never replayed, and the next one re-rendezvouses and replays the client handshake. No
  Node and no `mcp-remote`. The default role is still the existing in-process stdio
  gateway.

### P2.3 Lifecycle and failure (landed, #892, #893)

- Session lease on connect: the lease is the open connection. The adapter holds a long-lived
  `GET /mcp` listen stream while it is connected and deletes the session on client EOF, so
  per-session state is released at once. `reap_stale_mcp_sessions` still covers an adapter
  that dies without the DELETE.
- Daemon idle exit after `DAEMON_IDLE_GRACE`: landed (#892). It keys on nothing being in
  flight for the whole grace rather than on the session table, so a session row left behind
  by a crashed adapter cannot pin the process. Discovery is withdrawn before the exit is
  final and put back if a client connected in that window. `TOOLPORT_DAEMON_IDLE_GRACE_MS`
  overrides the grace for tests.
- Daemon crash: landed (#893). The affected call fails with an error and is never replayed;
  the next request re-runs the rendezvous and replays the client's `initialize` and
  `notifications/initialized`, so the replacement gets an equivalent session. Healthy calls
  share a read gate and run concurrently; recovery takes the write gate.
- Rollback: `--stdio-adapter` is opt-in; the legacy in-process role stays the default.

## Phase 3: downstream launch pooling

### P3.1 Union catalog with per-session scope

- Build the catalog once for the union of enabled servers and enforce each session's
  allowed set on every list, call, prompt, resource, subscription, and server-initiated
  path. The HTTP bridge already proves the filtering model; make it the only model.

### P3.2 Pool by `LaunchKey`

- Reuse one downstream launch per `LaunchKey` (server id plus launch-affecting fingerprint
  plus resolved root context). `${ROOT}` servers shard per distinct root; registry and
  secret generations retire the old key after in-flight calls finish.
- This is the measurable win: adding an ordinary client session must not add a router or a
  root-independent downstream copy.

### P3.3 Concurrency and isolation tests

- Concurrent clients with different identities, profiles, protocol eras, capabilities,
  roots, and overlapping request ids. Assertions must prove both sharing and isolation
  from the design's verification matrix.

## Phase 4: dogfood, default, convergence

- P4.1 registry feature flag, telemetry/diagnostics, and the real-machine acceptance run
  (heavy gateways, adapters, descendants, memory, cold-start and first-call latency).
- P4.2 default flip only after parity suites pass on Windows, macOS, and Linux, keeping a
  documented legacy kill switch for at least one release.
- P4.3 desktop Shared HTTP adopts a daemon service lease; app exit releases the lease
  instead of killing the process.

## Acceptance mapping

The design's verification matrix maps to: P2.1 (cold start election, mismatch isolation,
stale descriptor), P2.3 (crash, EOF, TTL cleanup), P3.3 (scope isolation, id collisions,
routing to the originating session), P4.1 (process and memory counts). The primary success
metric stays: another ordinary client session adds no router and no root-independent
downstream copy.
