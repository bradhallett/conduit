# Toolport codebase and product review

Reviewed September 5, 2026. This is a broad source and repository review, with
targeted verification. It is not a claim that every line or platform has been
exhaustively audited. No application code, published content, repository settings,
or deployments were changed.

## Scope and baseline

- Toolport: frontend, gateway/router, downstream transport, audit, client adapters,
  approval/quarantine surfaces, code mode, packaging, documentation, CI, open PRs,
  issues, and live main-branch protection. The checkout contains substantial
  pre-existing edits and untracked work.
- toolport-site: Astro pages/layouts/assets, Worker routes, CI, generated site
  references, and the live homepage. The Teams page and Worker have existing edits.
- toolport-teams: API/store architecture, analytics, web statistics, web tests and
  CI. Backend review was selective; SQLite/Postgres tests were not run locally.
- Existing PRs [#863](https://github.com/btsouth/toolport/pull/863) and
  [#862](https://github.com/btsouth/toolport/pull/862) already contain performance
  and cleanup work. Do not duplicate their changes. All returned checks on #863
  were successful at inspection, but the PR remains open.

## Findings, in recommended order

### 1. Restore a trustworthy integration baseline

The current checkout cannot compile its Rust test targets: 16 errors in
`src-tauri/src/agent_guard.rs`, starting around line 1724, refer to missing
`decision_response` or old function signatures. These are in pre-existing work.
The combined frontend verifier also stops on formatting in
`docs/screenshots/README.md`.

Finish or isolate the in-progress permission work before measuring new backend
changes. An isolated PR's green checks do not establish that this combined checkout
works. Do not delete the failing tests to obtain a pass.

### 2. Quarantine reads can misrepresent failures as an empty list

`src-tauri/src/integrity.rs:1559`, `all_quarantined`, ignores both read and JSON
parse errors for individual quarantine stores. It returns `Ok(out)` even when a
store was unreadable or malformed. The sidebar's error/stale UI cannot catch errors
that the backend discards.

Distinguish an absent store from an unreadable or malformed store. Return an error
or an explicit partial-result status, retain the last confirmed display, and test
ordinary missing, malformed, and inaccessible fixture stores. This is a UI
observability finding; it does not establish that gateway enforcement is bypassed.

### 3. GTK validation does not block merging

`.github/workflows/ci.yml` runs the GTK build/tests, but `merge-gate.needs` omits
`linux-native`. The live main protection requires only `Build + test`. Therefore
a failed GTK job alone does not prevent that required gate from succeeding.
`src/test/ci-required-checks.test.ts:22` mirrors the same omission.

If GTK is a supported shipping target, add it to the gate and its test. If it is
deliberately advisory, state that support boundary. Clippy is also outside this
gate, explicitly by design. Main does not require an approving review, strict
up-to-date checks, or admin enforcement; those are policy decisions, not automatic
cleanup changes.

### 4. Consolidate quarantine polling without losing notifications

`QuarantineAlert.tsx:86` polls every two seconds, while
`AppSidebar.tsx:421` separately reads the same list every ten seconds. These
schedule 36 reads per minute combined, excluding initial/manual refreshes and
runtime timer throttling. Each backend read loads the registry and profile stores.

Use one shared refresh source with no overlapping requests and distribute its
result to both views. Keep a trailing refresh after mutations. Crucially,
`desktop.rs:1638` also emits OS notifications from this read path. Simply pausing
all polling while hidden would remove background notification delivery. Move
notification detection into an owned backend task before applying visibility
gating, or retain the necessary background cadence.

This is a confirmed repeated-work opportunity, not a measured CPU saving.

### 5. Pending-approval refreshes lack response ordering

`PendingApprovals.tsx:100` starts reads from both a two-second interval and approval
events, then unconditionally applies results. A slow earlier response can replace
a later snapshot, temporarily resurrecting a resolved item or hiding a newer one.

Coalesce concurrent reads and preserve a trailing refresh after an event, or use
request generations. The existing `createSingleFlight` helper supports a trailing
read for mutation-sensitive callers. Test out-of-order completion with deferred
promises. Preserve authoritative broker decisions and countdown behavior.

### 6. Remove the tracked source archive from future trees

`packaging/linux/native/toolport-1.18.0.tar.gz` is a tracked 8,995,885-byte archive.
The PKGBUILD already downloads the versioned GitHub archive. This is a strong
generated-artifact removal candidate after checking packaging expectations; ignore
future local source downloads. Removing it reduces future checkout content, but
does not reclaim existing Git history. Do not rewrite history for this cleanup.

The 3,902,807-byte `docs/demo.gif` is another large artifact, but media with public
references should be migrated deliberately rather than treated as unused code.

### 7. Put test-only gateway helpers behind test configuration

Rust reports `ResourceSubscriptions::add` near gateway line 841 and
`mcp_push_server_message` near line 11800 as unused in production. Their references
are in tests. Gate them with `#[cfg(test)]`, or update tests to exercise the actual
production path if the wrapper no longer serves a distinct purpose.

This reduces misleading production surface and warning noise. Release dead-code
elimination may already remove them, so no binary-size improvement is claimed.

### 8. Correct the website's restart claim

The [live homepage](https://toolport.app/) says initial client toggles need no
restarts. `src/lib/clientConnect.ts` explicitly instructs users to restart a client
after connecting or removing Toolport. Separate initial client setup from later
changes propagated through an established gateway connection. Promise live updates
only for clients and actions that support them.

Also tighten causal language such as fewer tokens necessarily producing sharper
answers. Keep benchmark results tied to their workloads. The current page already
distinguishes one-request definition counts from graded task totals; retain that
useful distinction.

### 9. Test the website Worker, not just Astro compilation

The website's CI runs only `npm run build`. Its package has no test command, and no
test files were found outside dependencies/build output. The Worker handles
downloads, installer redirects, shared configurations, lead capture and license
fulfillment. An Astro build does not verify those request handlers.

Add isolated request/response tests with mocked services and storage, starting with
download asset selection, pinned installer redirects, share round trips, and billing
event classification/retry behavior. Use fixtures only; no live purchases, emails,
or webhook submissions are necessary. Existing Worker edits should be reconciled
before implementing this.

### 10. Consolidate repeated website layout and script code

The homepage, `SitePage.astro` and `BlogPost.astro` repeat navigation, metadata,
styles and GitHub star-count code. Analytics initialization is duplicated across
multiple pages. Build shared navigation/footer/metadata and analytics components,
keeping page-specific content and event names explicit.

The star fetch already has an hour-long localStorage cache. It is not an uncached
request on every navigation. A build-time value or edge cache is an optional way
to avoid each new visitor making a GitHub API request for a decorative counter.

### 11. Optimize the largest website images before adding frontend machinery

The Omarchy gallery contains PNGs of 815,926, 882,083 and 937,365 bytes. They are
lazy-loaded but have no responsive variants in their image elements. Generate
appropriate mobile/desktop formats and compare image quality and transferred
bytes. The homepage screenshot is only 186,450 bytes, so it is not the largest
opportunity. Fonts and the icon subset are already served locally. Keep the static
Astro architecture; this review found no justification for a framework rewrite.

### 12. Bound and reuse Teams analytics work

`toolport-teams/src/analytics.rs:48` spawns a blocking task and constructs a new
HTTP agent for each event. A five-second timeout bounds each request, not the
number of concurrently queued events. A bounded queue with an explicit drop policy
and a reused client would keep optional telemetry from competing with database
work during bursts. Measure event volume before selecting batching complexity.

`toolport-teams/src/webstats.rs:117` releases its cache lock before fetching, so
concurrent cache misses can issue the same upstream work. Coalesce refreshes and
serve the last good value while one refresh runs. No production load or latency
improvement was measured.

### 13. Treat same-server concurrency as an architectural experiment

`router.rs:1337` holds a per-server mutex through a downstream call. Different
servers can proceed concurrently; calls to one server serialize, including HTTP
transports using the same slot. This is a possible head-of-line delay for concurrent
HTTP workloads, not proof that all gateway calls are globally serialized.

Benchmark representative concurrent HTTP traffic before changing it. Session
state, reconnects, notifications, authentication and cancellation make a lock removal
unsafe as a cosmetic refactor. Preserve stdio request/response ownership.

### 14. Defer closed dialogs, then measure startup again

The current startup graph is 551,964 bytes raw / 170,486 gzip. Secondary destinations
already load lazily, but `App.tsx` eagerly imports ServerDialog and ImportReviewDialog.
Consider deferring their UI while keeping pure review helpers separate. Check first
open, keyboard focus, mutation state, and loading failures before claiming a win.
Re-measure the full static graph rather than merely splitting one file.

### 15. Reduce maintenance noise selectively

ESLint returns success with 53 warnings, including React ref/effect warnings as
well as fast-refresh export warnings. Triage behavior warnings first, then establish
a no-new-warnings baseline. Do not silence every warning globally.

Large source files are real maintenance costs: the gateway is about 29,800 lines,
and clients/downstream are each about 12,000. Much of that is tests. Extract coherent
protocol modules and test fixtures incrementally rather than calling all those
lines production bloat. Shorten comments that recount issue history, but retain
the explanations of ordering, compatibility, locking, and failure semantics.

The `shadcn` dependency is used by `src/index.css`; it is not unused just because
there is no JavaScript import. Legacy Conduit names also remain in crate/package
identities and compatibility paths. Neither is a safe blanket deletion.

### 16. Keep known code-mode resource limits ahead of cosmetic work

[Issue #759](https://github.com/btsouth/toolport/issues/759) already tracks missing
code-mode memory bounds. The module also documents that synchronous JavaScript is
not governed by a hard wall-clock interruption. Treat resource containment as an
existing reliability priority. Do not advertise a hard CPU/memory bound or remove
the existing limits as redundant. This review did not reproduce resource exhaustion.

## Verification performed

| Check                                        | Result                                              |
| -------------------------------------------- | --------------------------------------------------- |
| Toolport doctor                              | Passed prerequisite checks                          |
| Combined frontend verification               | Stopped at existing screenshot README formatting    |
| Separate production build and startup budget | Passed; 551,964 bytes raw, 170,486 gzip             |
| Frontend tests                               | 58 files, 656 tests passed                          |
| ESLint                                       | Passed with 53 warnings                             |
| Offline browser smoke                        | Passed Servers, Activity and logo fixtures          |
| Headless Rust check including test targets   | Failed: 16 existing agent-guard test compile errors |
| Website build                                | Passed; 54 generated pages                          |
| Generated-site local href/src/poster audit   | 1,521 references checked; zero unresolved paths     |
| Teams web tests                              | 19 passed after npm ci --ignore-scripts             |

No installed Windows/macOS/GTK runtime tests, production API profiling, Teams
database suite, purchase flow, or live Worker mutation tests were run. Local path
checking does not validate external URLs, fragment targets, or dynamic routes.
Build/test timings from this shared machine are not comparative performance claims.

Logs for this run are in `/tmp/toolport-review-*.log` and
`/tmp/toolport-teams-review-*.log`; browser artifacts are under
`.verify/browser-1788607732895-287964/`. The combined verification failure is under
`.verify/run-1788607657238-262660/`.

## Decisions needed before a larger cleanup

1. Review-only versus implementing clear fixes in isolated worktrees.
2. Any features, clients, platforms or legacy compatibility paths to retire.
3. Whether the main optimization target is desktop idle/startup, gateway throughput,
   maintenance/CI effort, or website/signup conversion.

Recommended order: reconcile existing PRs and the broken local test baseline;
correct misleading status/claims and merge gates; implement measured idle/startup
work; then tackle wider structural changes with representative workloads.
