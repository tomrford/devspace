# Synchronization and convergence

Devspace replicates a Jujutsu repository without replicating a working copy.
Each machine owns a bare Git repository and Jujutsu operation store. The cloud
owns immutable operation-store bytes and the authoritative set of operation
heads. Canonical Git objects live on a private Git remote.

The protocol is content-addressed and retry-safe. No synchronization request
rewrites an existing object.

## Commands and connectivity

`ds` records a successful repository mutation before it starts background
synchronization. The boundary process runs the same synchronization engine as
the hidden `ds sync run --repository-name <name>` command.

`ds sync status` reports whether the local daemon is running and, for each
machine repository, whether sync state exists and how many outbox entries are
pending. It does not report cloud heads or contact Git remotes.

Commands that need the cloud send the global development-only
`DEVSPACE_SHARED_SECRET` as a bearer credential, the repository incarnation in
`x-devspace-incarnation`, and a syntactically validated but non-authoritative
`x-devspace-machine-id`. The client capability header is
`x-devspace-client: ds/<version> git-remote/1`.

The shared credential selects one fixed development user. The control plane
checks that user's active repository incarnation before forwarding a request
to `Repository`; it does not validate machine enrollment. Per-machine
credentials are an open limitation, not part of the implemented protocol.

## Native repository boundary

Every checkout for one logical repository shares one bare Git object database.
It contains:

- canonical Git blobs, trees, and commits;
- public projection commits created for Git remotes;
- Jujutsu's operation and view files;
- `store/extra`, the rebuildable GitBackend metadata cache.

Only Git object bytes and operation-store objects are replicated. Workspace
state, working-copy files, locks, indexes, and `store/extra` stay local.

Opening a repository reconstructs missing GitBackend cache entries from the
Git commit headers. A fresh clone therefore needs no private side database to
recover change IDs or conflicted-tree metadata.

## Machine-store catalog

The machine store maps a validated repository name to:

- the cloud repository ID;
- its current incarnation;
- the local bare repository path.

Catalog creation is staged behind a durable creation intent. A crash after the
cloud allocates a repository but before local materialization resumes the same
intent. It does not allocate another repository. Per-repository locks serialize
materialization, synchronization, and destructive removal.

The cloud control plane is the authority for names and incarnations. A deleted
incarnation cannot authorize an old repository Durable Object.

## Git object closure

The Git closure starts from commit heads and walks:

- commit to root tree and parent commits;
- tree to every referenced blob or child tree.

Object keys are `(kind, oid)`, where kind is blob, tree, or commit and the OID
is the 20-byte Git SHA-1. The validator recomputes the exact Git object
preimage, rejects collisions, parses structured objects, and records their
references.

Closure discovery rejects:

- a missing referenced object;
- a referenced object of the wrong kind;
- malformed Git bytes;
- an object whose supplied OID does not match its bytes;
- an object beyond the configured byte or closure bounds.

The machine reads canonical and projected objects from the same Git object
database.

## Canonical Git remote

Git objects travel through standard Git fetch and push against a private
remote. Tests use a local bare repository. A live Artifact repository uses a
repository-scoped token. The Worker does not store Git object bytes.

Each machine writes only its own retention ref
`refs/heads/__devspace/machines/<machine-id>`. The ref points at a synthetic
commit whose parents include the previous anchor and the required canonical
heads. Extra objects that standard Git reachability would not send, including
`jj:trees` trees, are named from a carry tree on that commit. Fetch maps those
refs into `refs/devspace/retention/*` so they stay out of the user's
`refs/heads` view.

A machine-owned ref is the selected retention design. A repository-wide
compare-and-swap ref serializes unrelated machines. Direct commit refs do not
retain `jj:trees` extras.

## Operation object closure

Jujutsu operation history uses a separate content-addressed graph:

- an operation references its parent operations and one view;
- a view references canonical Git commits and other jj view state.

View and operation IDs are 64-byte Blake2b values. The machine validates local
objects before upload. The Worker validates them again before insertion.

Inventory requests are bounded batches of `(kind, id)` keys. The Worker returns
the keys already present, and the machine computes which objects are missing
and uploads only those objects. Git commit references in views must be
durable on the canonical Git remote before an operation head can advance.

## Cloud operation heads

The cloud stores a set of operation heads and a monotonically increasing
cursor. A head transaction contains:

- a stable idempotency key;
- one proposed new head;
- the previously observed heads that the new operation supersedes.

The Worker verifies that the new operation and its closure are durable,
removes only observed ancestor heads, inserts the new head, and records the
result under the idempotency key. Replaying the same request returns the same
result. Reusing a key for different bytes is rejected.

Concurrent machines can add distinct heads. They do not overwrite one another.
The next machine to synchronize downloads both closures and lets Jujutsu create
the native merge operation.

## Synchronization run

One run holds the repository synchronization lock and follows this order:

1. load local sync state;
2. if an outbox batch exists, push and verify its reachable Git objects, upload
   its operation closure, replay its head transactions, and stop;
3. read cloud operation heads;
4. fetch Devspace retention refs from the canonical Git remote;
5. download the missing operation closures;
6. reconcile multiple cloud heads in the native Jujutsu repository;
7. persist the accepted heads;
8. discover the current local operation heads;
9. push this machine's retention root and verify a fresh fetch can read every
   required Git object;
10. upload their operation closure;
11. write a durable outbox batch for new head transactions;
12. apply each transaction and remove each acknowledged outbox entry.

The outbox is written only after every referenced Git object is durable on the
canonical remote and every referenced operation object is durable in the
Worker. On retry, the machine pushes and uploads again before replaying the
transaction. This ordering makes a local crash, network timeout, or lost
response recoverable without guessing whether the cloud committed. Extra
immutable Git objects after a crash are acceptable.

## Convergence

Objects converge by immutable content identity. Operation heads converge by
set reconciliation.

If machines A and B write concurrently:

1. A and B upload disjoint objects and operation heads;
2. the cloud retains both heads;
3. a later sync downloads both operation closures;
4. Jujutsu merges the operations locally;
5. that merge operation is uploaded as a new head;
6. its transaction removes the observed ancestors.

No last-writer-wins register exists for repository state. A transaction can
remove only heads it proves it observed.

## Exact cloud rebuild

A new machine can recover a repository using only:

- its control-plane repository identity;
- the canonical Git remote and its retention refs;
- operation objects and cloud operation heads.

It fetches the retention refs, downloads each operation closure, rebuilds
the GitBackend cache from commit bytes, and opens a checkout at the recovered
operation. The recovered canonical Git OIDs and Jujutsu operation IDs match the
source machine exactly.

Projection journal state is cloud data in the same repository Durable Object.
It is needed for Git remote continuity, not for canonical repository recovery.

## Command-boundary recovery

Mutation commands recover the native repository before exposing it to
Jujutsu. They serialize against sync, finish any durable operation-head outbox,
and reject an inconsistent or retired repository identity.

Git push and fetch add their own projection-journal recovery boundary. See
[Git push](git-push.md) and [Git fetch](git-fetch.md).

The following are deliberate hard failures:

- installed bytes conflict with an existing object ID;
- a pack or operation closure is incomplete;
- cloud authorization names a stale incarnation;
- an idempotency key is reused for a different request;
- projection state would bind one canonical commit to two public commits;
- hidden-path scanning detects public disclosure.
