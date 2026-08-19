# Artifacts Git store spike

## Purpose

This spike tests whether Cloudflare Artifacts can replace Devspace's custom
cloud Git-object transport without changing Devspace's Jujutsu model. The
machine remains a normal jj repository. Git objects move through the standard
Git smart HTTP protocol. Devspace continues to synchronise jj operation and
view objects, and its Durable Object continues to coordinate the repository's
set of operation heads.

The spike starts from commit `6cdc453` on branch
`spike/artifacts-git-store`. It is a compatibility and failure-semantics test,
not a production migration.

## Decision boundary

Continue with Artifacts only if the spike proves all of these properties:

- a repository can be rebuilt on a fresh machine from the Git remote and the
  Devspace operation store;
- every Git object referenced by an advertised jj operation survives remote
  garbage collection;
- concurrent machines can upload without serialising unrelated Git object
  transfer;
- a crash at every write boundary is recoverable by retrying the existing
  durable outbox;
- Git object bytes and object IDs remain exact, including the jj GitBackend
  commit forms covered by the kernel golden vectors;
- `.dsprivate` paths never enter the publishable Git repository; and
- the live Artifact behaves like the local bare Git test remote for the
  protocol features on which Devspace depends.

Stop if object retention needs a service-side proxy, if normal Git operations
cannot preserve the required object closure, or if the resulting state machine
is more complex than the current pack transport.

## Scope

The spike replaces only the cloud transport for canonical Git objects. It does
not:

- store jj operation or view objects in Git;
- make the publishable projection the canonical repository;
- implement Josh-style server-side path projection;
- make Devspace another general-purpose Git host;
- deploy a production Worker, migrate existing data, or preserve the current
  `DSPK` protocol for backwards compatibility; or
- introduce a generic storage-provider abstraction before a second provider
  exists.

The canonical private Git repository and any publishable Git repository are
separate repositories with separate credentials. Artifact tokens are scoped
to a repository, not to a ref, so ref naming is not a privacy boundary.

## Target shape

| Concern | Current implementation | Spike target |
| --- | --- | --- |
| Canonical Git objects | Deterministic `DSPK` v2 packs stored and validated by `Repository` | Standard Git fetch and push against a private remote |
| Local test remote | Devspace Worker | Bare Git repository started by the test harness |
| Live compatibility remote | Devspace Worker | One disposable Cloudflare Artifact repository |
| jj operation and view objects | `Repository` Durable Object | Unchanged |
| jj operation heads | Optimistic transaction in `Repository` | Unchanged |
| Crash recovery | Durable machine outbox | Extended so the Git push completes before head publication |
| Public Git projection | Local projection plus normal Git push and fetch | Unchanged |

Cloudflare does not currently emulate Artifacts in Miniflare. Normal tests
must therefore use a local bare Git remote. One ignored live test supplies the
Artifact URL and repository token through environment variables. This keeps
the main suite deterministic and unauthenticated while still testing the real
service boundary.

## Invariants

The implementation must keep these invariants explicit:

1. A jj operation head is never published before all Git objects and all jj
   objects reachable from it are durable.
2. Git objects are immutable and are identified by their standard object ID.
   Devspace does not re-encode them in transit.
3. jj operation and view objects keep their canonical protobuf bytes and
   semantic Blake2b IDs.
4. The Durable Object's operation-head set remains the authority for jj
   convergence. A Git ref is a retention root, not a replacement operation
   head.
5. Concurrent machines may add objects and retention roots. They must not
   overwrite another machine's root.
6. Retrying any interrupted upload or head transaction is safe.
7. Private and publishable repositories have independent URLs and
   credentials. No canonical-private credential is passed to projection code.

## Retention experiment

Git servers retain objects that are reachable from refs. jj can reference Git
commits which are not on a conventional branch, so pushing only user-facing
branches is insufficient.

Test a machine-owned retention ref of the form:

```text
refs/heads/__devspace/machines/<machine-id>
```

The ref points to a standard synthetic Git commit. Its parent list includes
the previous anchor and the new canonical Git commits required by the jj
operations being uploaded. Each machine writes only its own ref. The anchor
tree is empty and the commit message contains no private content.

Selected design: machine-owned synthetic ancestry. A repository-wide
compare-and-swap ref lost because the next writer must already hold the
previous synthetic commit, so unrelated machines serialize. Direct commit refs
lost because Git reachability does not follow `jj:trees`.

The spike compared this design with two alternatives:

- one repository-wide retention ref updated with compare-and-swap retries;
- direct pushes of the required commits to machine-owned refs without a
  synthetic ancestry chain.

Measure ref growth, push cost, fetch negotiation, concurrent updates, forced
garbage collection where the service permits it, and deletion behaviour. Do
not select a design only because the happy path works.

## Upload transaction

Extend the existing outbox transaction rather than adding a second journal.
For each pending operation-head change:

1. compute the new jj operation and view object closure;
2. compute the referenced canonical Git object closure;
3. create or advance this machine's retention root locally;
4. push the retention ref to the canonical private Git remote;
5. verify that the remote accepts the update and that a fresh fetch can obtain
   every required object;
6. upload the jj operation and view objects to the Durable Object;
7. write or retain the durable outbox entry;
8. transact the operation-head set; and
9. clear the acknowledged outbox entry.

The durable record must contain enough information to resume at any boundary.
If a crash leaves extra Git objects or an advanced machine retention ref, a
retry may reuse them. Extra immutable objects are acceptable; a published jj
head with missing data is not.

## Download and recovery

A fresh machine must be able to recover without access to another machine:

1. create the stock jj GitBackend repository layout;
2. fetch the Devspace retention refs from the canonical private Git remote;
3. download the jj operation and view closure from the Durable Object;
4. verify all object IDs and reference edges;
5. install the operation-head set; and
6. materialise the working copy through jj.

After recovery, remove or hide transport-only refs from the user's normal ref
view. Do not rewrite canonical commits or jj operation objects.

## Authentication

Use two credential classes:

- `CLOUDFLARE_API_TOKEN` is an account API token limited to
  `Artifacts: Edit` on the single Cloudflare account. Wrangler uses it to list,
  create, inspect, and delete disposable Artifact repositories and to mint
  repository tokens.
- Git uses a repository-scoped Artifact token. Give the spike read-write access
  only to its disposable canonical repository. Use a separate token if a live
  publishable repository is tested.

Set `CLOUDFLARE_ACCOUNT_ID` beside the account token in the Cursor environment.
Keep all tokens in the task's secret environment. Do not put them in
`wrangler.jsonc`, `.dev.vars`, command-line arguments, test snapshots, logs, or
the repository. Redact authenticated remote URLs in errors.

The account token cannot be limited to one Artifact repository. Its narrowest
useful boundary is `Artifacts: Edit` on one account. The repository token is
the repository-level boundary for Git data access.

## Work plan

### 1. Record the baseline

- Run `nix develop -c sfw pnpm check` and
  `nix develop -c sfw pnpm test` before code changes.
- Trace the current pack upload, pack download, outbox, operation-object, and
  operation-head paths. Record each mutation boundary in the implementation
  notes for the first code change.
- Confirm the exact Wrangler v4 Artifacts commands from the installed version
  and current Cloudflare documentation before scripting live setup.

### 2. Prove standard Git compatibility locally

- Add a test fixture which creates a bare Git repository and exposes its URL
  to the machine sync code.
- Push and fetch all canonical Git golden vectors through that remote.
- Cover commits with unknown headers, conflict labels, non-UTF-8 data,
  signatures and mergetags where the current fixtures support them.
- Prove that a fresh clone plus fetched retention refs contains every object
  referenced by the corresponding jj operations.

Use the existing Git process environment and command runner. Add one focused
module for the canonical remote operations; do not add a provider interface.

### 3. Prove retention and concurrency

- Implement the three retention candidates behind test-only selection.
- Run two-machine and many-machine push races.
- Test non-fast-forward rejection, retry, stale local state, deleted refs, and
  interrupted pushes.
- Choose the smallest design which keeps all required objects reachable and
  does not create a repository-wide serialisation point.
- Delete the losing candidates before continuing.

### 4. Join Git durability to the existing outbox

- Add the selected retention update to the pending operation-head batch.
- Make Git durability a prerequisite for operation-object upload and head
  publication.
- Add failpoints after each numbered upload step and recover from each state on
  the next sync.
- Preserve the current optimistic operation-head transaction and merge
  behaviour.

### 5. Remove the replaced transport

- Remove the `DSPK` upload and download paths, pack manifests, Worker Git-pack
  routes, and Worker Git-object storage which no longer have a caller.
- Keep kernel Git parsing and validation used by the jj format boundary and
  projection. Delete code based on callers, not on the old module name.
- Update the current contract documents in the same change. Do not leave a
  migration path or compatibility branch.

### 6. Re-run projection privacy tests

- Push a repository containing tracked `.dsprivate` paths to the canonical
  private remote.
- Publish its filtered projection to a separate bare remote.
- Assert by object traversal, not only by checkout, that hidden paths and blobs
  are absent from the publishable repository.
- Re-run push, fetch, force-push, delete and signed-rewrite tests.

### 7. Test one live Artifact

- Create one disposable private Artifact repository with Wrangler.
- Mint a repository-scoped read-write Git token.
- Run the same compatibility, retention, concurrency and fresh-recovery suite
  selected by an explicit live-test environment flag.
- Record service-specific limits and errors as test assertions where stable.
- Delete the disposable repository after the result has been captured.

The live test must answer these open questions:

- Does Artifacts accept and advertise the selected retention ref names?
- Does it preserve every required object and header through push and fetch?
- What are its non-fast-forward and concurrent-update semantics?
- When and how does unreachable-object garbage collection occur?
- Can repository tokens be minted with a useful lifetime for unattended tasks?
- Which size, request, ref-count or pack limits affect a realistic Devspace
  repository?

## Verification matrix

| Case | Local bare remote | Live Artifact |
| --- | --- | --- |
| Exact Git object round trip | Required | Required |
| Fresh-machine jj recovery | Required | Required |
| Two-machine convergence | Required | Required |
| Crash at each outbox boundary | Required | One representative run |
| Retention after maintenance or GC | Forced locally | Observe or invoke if available |
| `.dsprivate` object absence from public remote | Required | Optional service check |
| Large history and pack negotiation | Required | Required at a bounded fixture size |

Before hand-off, run the full repository gate:

```sh
nix develop -c sfw pnpm check
nix develop -c sfw pnpm test
```

Do not deploy Devspace from this spike. The result is a measured decision: a
small implementation diff plus evidence that Artifacts can satisfy the
invariants, or a short rejection which identifies the failed invariant.

## Live Artifact findings

A disposable private Artifact accepted machine retention refs of the form
`refs/heads/__devspace/machines/<machine-id>` and advertised them on
`ls-remote`. Exact Git bytes, including `jj:trees` extras on the carry tree,
round-tripped. Two machines pushed concurrent retention refs without
serializing. A create-lease against an existing retention ref was rejected.
A 24-commit incremental push then fetched on a fresh machine.

Repository write tokens can be minted with an explicit TTL. The plaintext
token includes a `?expires=` suffix, so it must be percent-encoded in Git
URL userinfo. Artifacts rejects `--atomic` push and hangs up; a single-ref
`--force-with-lease` push is enough for machine-owned retention. Unreachable
object GC was not exposed as a client control. Full live jj recovery still
needs a Worker operation store beside the Artifact; this spike did not
deploy one.
