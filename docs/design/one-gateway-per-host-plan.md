# Implementation plan: one heavy gateway per host

Companion to [`one-gateway-per-host.md`](one-gateway-per-host.md). That document is the
design; this one is the build order. It is a multi-PR architecture change, not a single
gateway edit, and it should be read as a checklist rather than a commitment to land it in
one release.

## Current state

Grounded in the code as of this plan:

- Phase 0 landed. `src-tauri/src/topology.rs` defines `GatewayRole` (including the not yet
  constructed `Daemon` and `StdioAdapter`), `CompatKey`, `LaunchKey`, `TopologySnapshot`,
  and the topology assertions. Nothing else in the binary uses the module yet.
- The SBS-551 slice exists: `ActiveRequestContext` and its guards
  (`toolport-gateway.rs:78` onward, `thread_local! ACTIVE_REQUEST_CONTEXT`). The design
  calls this "the first boundary needed by the shared host daemon".
- `GatewayState` (`toolport-gateway.rs:10904`) still mixes host and session state, and a
  set of process globals still encode one-stdio-client assumptions (discovery/code mode,
  stdio presence and era, progress routes, PII maps, modern HITL approvals, result stash,
  `SearchGuard`/`ConfirmGuard`).
- There is no `HostState`, `SessionState`, or `RequestContext` type, and no topology
  feature flag in the registry.
- Reusable primitives already exist: the approval broker's `EndpointDescriptor` pattern
  (`approval.rs:21`), `registry::atomic_write` (`registry.rs:377`), and the registry
  cross-process `FileLock` (`registry.rs:3168`).
- Client launch: `clients.rs:gateway_entry` builds the stdio entry and sets
  `TOOLPORT_CLIENT_ID` (`clients.rs:5353`). The desktop app spawns `--http <port>` through
  `http_bridge.rs:start_with_token_at` and kills it on exit (`desktop.rs:4569`).

## Delivery shape

| PR  | Slice                                                               | Behavior change           |
| --- | ------------------------------------------------------------------- | ------------------------- |
| 1   | P2.1 rendezvous primitives (library module, tested)                 | none (new module only)    |
| 2   | P2.2a identity role; P2.2b host runtime on the internal endpoint    | none (explicit flag only) |
| 3   | P1.2 `SessionState` extracted from `GatewayState`                   | none                      |
| 4   | P1.3 `HostState` extracted; `GatewayState` becomes a thin facade    | none                      |
| 5   | P2.2c stdio adapter speaks the daemon session protocol, behind flag | opt-in only               |
| 6   | P2.3 session lifecycle, TTL, crash/EOF handling, fallback           | opt-in only               |
| 7   | P3.1 union catalog built once, allowed-set enforced per session     | opt-in only               |
| 8   | P3.2 downstream pooling by `LaunchKey` and `${ROOT}` sharding       | opt-in only, the big win  |
| 9   | P4.1 dogfood flag, telemetry, acceptance run                        | opt-in only               |
| 10  | P4.2 adapter topology becomes default; legacy kill switch remains   | default flip              |
| 11  | P4.3 desktop Shared HTTP converges onto a daemon service lease      | separate, later           |

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

What remains is not per-request but session- and host-scoped: `MODERN_STDIO_UPSTREAM`,
the `STDIO_*` handshake flags, `PROGRESS_*`, the PII map, and the modern HITL approval
table. Those are single-stdio-client assumptions and move in P1.2 (`SessionState`) and
P1.3 (`HostState`), so the isolation work lands with the types that own it instead of as a
mechanical rewrite of the request path.

### P1.2 SessionState

- Introduce `SessionState` owning exactly what the design lists: session id, principal and
  audit label, effective scope, protocol version and capabilities, roots and `${ROOT}`,
  upstream request correlation, outbound queue, cancellation, subscriptions, search guard,
  confirm guard, connection-local notification eligibility.
- `McpSession` becomes the HTTP transport face of `SessionState`; the stdio path gets the
  same type with a stdio transport face, deleting `StdioUpstream` as a separate concept.
- PII maps, shaped-result cursors, and modern HITL approvals stay keyed by principal, but
  move behind a `SessionStore` with explicit TTL and cap, with tests for reap-on-close and
  reap-on-TTL.

### P1.3 HostState

- Introduce `HostState` owning registry and watcher, router and rebuild lock, catalog
  snapshot and cache writes, downstream pool and circuit-breaker state, quarantine and
  rate-limit bindings, audit/metrics/savings, `server_handler`, and
  `resource_updated_sink`.
- `GatewayState` shrinks to a facade holding `HostState` plus a `SessionStore`, so the
  existing 300 call sites keep compiling while P2 moves ownership.
- Tests: the topology assertions in `topology.rs` stay green; a host with one router and
  two sessions reports `router_owners == 1`.

## Phase 2: rendezvous and the stdio adapter

### P2.1 Rendezvous primitives (this PR)

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

- P2.2a (landed): `--daemon` is accepted and the P2.1 identity listener serves
  `/host/identity`.
- P2.2b (this PR): `--daemon` runs the full host runtime on an ephemeral loopback endpoint
  with a random internal bearer and publishes the descriptor. The internal
  `/host/identity` route is daemon-only, so the user-facing HTTP bridge never exposes the
  compat fingerprint or build. Still explicit-flag only, off the default startup path, and
  with the same registry, router, watcher, audit, and session tables as the HTTP bridge.
- P2.2c: `--stdio-adapter` performs `Rendezvous::ensure` and speaks the daemon session
  protocol with Toolport's Streamable HTTP/SSE: one session open, bidirectional JSON-RPC
  translation, cancellation, oversized frames, and server-initiated RPC correlated back to
  the originating session. No Node and no `mcp-remote`. Never fall back to the in-process
  gateway after a request may have reached the daemon. Until it can serve, the default role
  stays the existing in-process stdio gateway.

### P2.3 Lifecycle and failure

- Session lease on connect, immediate close on adapter EOF, TTL reaping for crashed
  adapters with the same subscription/PII/confirmation cleanup as an explicit close.
- Daemon idle exit after a testable grace period, only with no leases, in-flight calls,
  pending approvals or server requests, and no active subscriptions.
- Daemon crash: fail the affected in-flight request with a clear Toolport error, re-rendezvous
  before the next request, never replay an ambiguous call.
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
