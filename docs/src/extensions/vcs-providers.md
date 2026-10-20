---
title: External VCS Providers
description: Experimental external version control providers
---

# External VCS Providers

This experimental API lets an extension supply repository discovery, changed files,
and comparison contents to Zed's native source-control UI. Git keeps its existing
backend. A provider does not emulate Git or create `.git` metadata.

The API is local. Protocol 1 is read-only; protocol 2 can explicitly enable
selected-file staging, unstaging, discarding unstaged changes, scoped commits, and
branch switching, and checkout catalog/create/open/prepare/suspend/remove operations.
Agent archive and remote transport are not provided.
Read-only capabilities constrain Zed's calls; they are not an operating-system
sandbox for the provider executable. Zed requires workspace trust before invoking
the extension callback or starting its process.

## Extension registration

An extension using development API 0.9 declares providers in its manifest:

```toml
[vcs_providers.example]
root_markers = [".example-repository"]
```

Markers are filenames, checked in the opened directory and its ancestors without
recursively scanning those ancestors. They identify candidates; the provider must
confirm repository ownership. An existing nested Git repository retains ownership
of its files. Ambiguous external ownership must be reported, not resolved by
registration order.

Implement `Extension::vcs_provider_command` to return the executable, arguments, and
environment. This callback follows the language-server command pattern, but the
returned process speaks the VCS protocol below. The native host owns its pipes,
requests, cancellation, and lifetime. `process::Command::output` is not a streaming
transport and must not run the provider loop from inside WASM.

## Wire protocol

Messages use JSON-RPC 2.0 with `Content-Length: <UTF-8 byte count>\r\n\r\n`
framing. Headers are bounded to 8 KiB and each body to 16 MiB. Diagnostics belong
on stderr; stdout contains protocol frames only. Clients generate integer request
IDs. Providers send responses and notifications, but cannot request client actions.

All model fields use camelCase. Repository, revision, snapshot and baseline IDs are
opaque strings; an identifier such as `r42` is valid. Paths use slash-separated
components relative to `scopeUri`, not the repository root. Absolute paths, empty
components, `.`/`..`, NUL, backslashes and colon-containing components are rejected
by the initial portable-path contract. Report an unsupported path rather than
silently omitting it. File URIs identify local paths only.

| Method            | Parameters                                                                    | Result                                          |
| ----------------- | ----------------------------------------------------------------------------- | ----------------------------------------------- |
| `initialize`      | `protocolVersion`, `supportedProtocolVersions`, `clientInfo: {name, version}` | `protocolVersion`, `serverInfo`, `capabilities` |
| `vcs/discover`    | `scopeUri`                                                                    | Array of repository descriptors                 |
| `vcs/snapshot`    | `repositoryId`, `baselineId`                                                  | A snapshot of changes for that comparison       |
| `vcs/readContent` | `repositoryId`, `snapshotId`, `baselineId`, `path`                            | Baseline-side content                           |
| `shutdown`        | Empty object                                                                  | `null`, after provider cleanup                  |

After shutdown, the client sends an `exit` notification. Dropping a request sends
`$/cancelRequest` with its `id`. Shutdown and process exit have bounded deadlines;
the host terminates and reaps its owned process group if the provider does not
finish. Providers must keep their subprocesses in that ownership boundary and
cancel their own outstanding work on shutdown or stdin EOF.

The optional `vcs/didChange` notification has `{repositoryId}` parameters. It
invalidates the visible snapshot; Zed also refreshes after relevant file changes
and supports explicit refresh. Request failure, timeout, malformed data and process
EOF are errors, not empty successful snapshots. Error `code`, `message`, and
optional `data` retain provider details. Responses received before EOF remain valid.

### Version negotiation

Zed sends `protocolVersion: 1` and `supportedProtocolVersions: [1, 2]`.
Existing protocol 1 providers keep returning `capabilities: {readOnly: true}`.
Protocol 1 cannot advertise file operations. A protocol 2 provider opts in with:

```json
{
  "protocolVersion": 2,
  "serverInfo": { "name": "Example", "version": "2" },
  "capabilities": {
    "readOnly": false,
    "fileOperations": ["stage", "unstage", "discard"]
  }
}
```

Missing `fileOperations` means no file operations. Setting `readOnly: false`
does not enable any operation by itself. A read-only provider must not advertise
file operations; contradictory capabilities are rejected.

### Selected-file operations

The Source Control panel shows Stage File, Unstage File, and Discard Unstaged for the selected
change. The optional `fileOperations` on each change narrows provider-level capabilities;
an empty list disables all writes for that row. An optional `unavailableReason` explains
why. Without this field, only an ordinary modified file without a rename retains legacy
file operations. Text preview support does not determine file-operation support.
Unsupported actions are disabled. Zed checks workspace trust,
provider ownership, and open buffers before preparation and again before applying
the operation. Save or revert unsaved edits and resolve disk conflicts first.
File operations do not enable hunk actions in the editor.
Discard preserves staged changes. Depending on the selected change it can restore the
index version or remove an untracked file. The confirmation includes the provider's
prepared summary and every affected path. Zed prepares the operation before asking for confirmation, so a file
changed while the confirmation is open cannot be discarded using newer,
unreviewed state.

| Method                       | Parameters                          | Result                                       |
| ---------------------------- | ----------------------------------- | -------------------------------------------- |
| `vcs/prepareFileOperation`   | `repositoryId`, `path`, `operation` | `{operationToken, affectedPaths?, summary?}` |
| `vcs/applyFileOperation`     | Same fields plus `operationToken`   | `{outcome}`                                  |
| `vcs/reconcileFileOperation` | Same fields plus `operationToken`   | `{outcome}`                                  |

`affectedPaths` defaults to the requested path for older providers. It must contain the
requested path and may additionally include its known rename source. The host validates
ownership and checks buffers for all affected absolute paths in every open Project.
Providers must bind the complete set into the token and mutate rename pairs together.
If a historical comparison rename no longer represents a current operation pair,
its row must disable file operations with an explicit reason.
Explicit per-row capability can permit absent files, binary files, symlinks and mode
changes. The host still rejects directories and symlink parents; providers must validate
exact content, type, mode and absence again before writing.

`operation` is `stage`, `unstage`, or `discard`. Preparation is read-only. The provider
returns an opaque, nonempty token of at most 4 KiB that binds the repository,
scope, exact file, operation, expected state and desired result. The provider must
check the expected state again before writing, serialize conflicting operations
across its processes, and verify the persisted result. It must reject unsupported
file kinds and changes explicitly, preserve unrelated files, and prevent a stale
request from overwriting newer state. A preflight read alone is not an atomic
compare-and-swap guarantee against other writers.

`outcome` is `applied`, `notApplied`, or `conflict`. Every terminal reconciliation
consumes the token, including `notApplied` for an operation that never started.
A subsequent attempt requires fresh preparation. Reconciliation is recovery, not
a preview that leaves execution rights intact. Terminal observations must be
persisted before returning, even if another operation produced the desired state.
Repeating an applied token must not apply the operation again. Reconciliation reads the actual state and
must work after the provider restarts. A conflict means the current state no
longer matches the prepared operation; review the file before preparing another.
Each preparation needs a distinct identity and durable replay protection:
file contents can return to their earlier values after an intervening operation,
so content fingerprints alone cannot identify a previously applied request.

Timeout, cancellation, process exit and malformed replies do not establish that
an operation failed to write. Zed reconciles the same token after an apply error.
If reconciliation also fails, further writes are blocked for that repository
until Refresh resolves the outcome. Closing the initiating panel does not cancel
its in-flight reconciliation. Before applying an operation, Zed persists its token
in the current user profile. An unresolved operation remains blocked across
provider reconnects, folder reopening, and Zed restarts until reconciliation
establishes its outcome.

### Repository operations

Protocol 2 providers can separately advertise
`repositoryOperations: ["commit", "switchBranch"]`. Missing capabilities disable
these actions. Providers that have no staging model do not need to offer commit.

| Method                             | Parameters                             | Result                                     |
| ---------------------------------- | -------------------------------------- | ------------------------------------------ |
| `vcs/repositoryState`              | `repositoryId`                         | `{currentBranch, branches, stagedPaths}`   |
| `vcs/prepareRepositoryOperation`   | `repositoryId` and the operation below | `{operationToken, summary, affectedPaths}` |
| `vcs/applyRepositoryOperation`     | Same request plus `operationToken`     | `{outcome}`                                |
| `vcs/reconcileRepositoryOperation` | Same request plus `operationToken`     | `{outcome}`                                |

Each branch has an opaque `id` and a display `name`. `currentBranch` can be
`null`. Paths remain relative to the opened scope. Zed loads repository state
alongside its existing snapshot refresh; an unavailable branch list does not
invalidate a usable file diff.

A commit request is `{"operation":"commit","message":"Commit message"}`. Its
prepared `affectedPaths` must list every staged file that will be committed.
The provider must refuse a commit that would silently include staged changes
outside the opened scope or files owned by a nested VCS. It must freeze the full
relevant staged set and content, recheck it under its shared mutation lock, commit
the staged contents, and verify the resulting commit. Newer working-file changes
remain on disk. The confirmation shows the exact project, scope, message, and
prepared file list.

A branch request is `{"operation":"switchBranch","branchId":"opaque-branch-id"}`.
Switching affects the whole checkout. Zed checks open projects and unsaved or
conflicted buffers; the provider must also verify checkout-wide dirty state and
other active sessions. A failed or conflicting switch must not be reported as
success.

File and repository operations share one recovery journal. A repository-wide
intent conflicts with unresolved file operations in other scopes of the same
provider and checkout, and blocks their new writes until it is settled. The
journal reads the previous file-only format and normalizes equivalent file URIs.
Reconciliation can rediscover the original scope without opening another project.
An uncertain result is an error and retains the pending token; `conflict` denotes
verified divergent state with no writer still running.
An executing operation holds an in-memory ownership claim shared across Zed
projects. Another window cannot reconcile it before dispatch or while it is
running. Dropping that owner releases only the claim; its unresolved durable
intent remains available for recovery after a process restart.

### Checkout catalog and lifecycle (version 2)

`checkoutCatalog: true` enables `vcs/listCheckouts`; `checkoutOperations` separately
lists `create`, `open`, `prepare`, `suspend`, and `remove`. A read-only catalog can
include foreign entries without allowing mutations. Every row has an opaque `id`,
`name`, `rootUri`, `scopeUri`, optional `branch`, `owned`, `state`
(`mounted`, `unmounted`, `missing`, or `busy`), allowed `operations`, and an optional
`unavailableReason`. Preparation is independently `ready`, `preparing`, or `failed`,
with an optional `preparationError`.

| Method                           | Parameters                          | Result                                                |
| -------------------------------- | ----------------------------------- | ----------------------------------------------------- |
| `vcs/listCheckouts`              | `repositoryId`                      | `{checkouts: [...]}`                                  |
| `vcs/prepareCheckoutOperation`   | `repositoryId`, tagged operation    | `{operationToken, summary, targetRootUri, checkout?}` |
| `vcs/applyCheckoutOperation`     | same operation and `operationToken` | `{outcome, checkout?}`                                |
| `vcs/reconcileCheckoutOperation` | same operation and `operationToken` | `{outcome, checkout?}`                                |

Create carries `operation: "create"`, `name`, `branch`, and `base`. Other requests
carry `operation` and `checkoutId`. The mandatory `targetRootUri` reserves the exact
target before dispatch, including before creation. Zed persists it and blocks
conflicting source or target operations across Projects. Identity and scope must
survive suspension and remount; a missing entry must never silently create a new
checkout.

`vcs/operationProgress` notifications carry `repositoryId`, `operationToken`,
`phase`, and `message`. Zed only displays progress for the currently owned token.
Creation and readiness preparation are separate phases. If creation succeeds but
preparation fails, return `applied` with the same checkout ID and failed preparation.
The catalog exposes Prepare to retry that checkout; Zed retains it and never
performs automatic filesystem rollback. Project setup is provider-specific and
must not be implied by readiness alone.

Suspend must preserve the checkout's data and branch. Remove is a separate,
confirmed action. The provider must check ownership, other users and all data
(including ignored files, local commits and stashes) across the whole checkout.
Zed also refuses Suspend/Remove while any live Project overlaps its root. A
provider may decline removal when cleanliness cannot be proved. The host never
falls back to deleting the directory after a provider refusal.

The native Checkouts picker is available from Source Control, the title bar, and
`vcs::OpenCheckouts`. It requires a live provider repository as its catalog anchor.
Opening uses the returned scope and the ordinary workspace flow. Provider-created
workspaces inherit trust through that flow without running Git create-worktree
hooks. Agent creation supports one local provider scope and rejects mixed Git/provider projects
rather than claiming isolation for folders retained from the source workspace.
Git checkpoints/archive remain unavailable where unsupported; ordinary agent
prompts continue without a checkpoint.

The known/unknown outcome rules above also apply here. A started writer whose
state is still uncertain must return an `outcomeUnknown` error and retain its token,
not a terminal `conflict`. Reconciliation must not retry creation or delete a
partially created checkout. A verified divergent state may become a terminal
conflict only after the writer is known to have stopped.

### Repositories and scopes

```json
{
  "id": "example:checkout-1",
  "rootUri": "file:///workspace",
  "scopeUri": "file:///workspace/app",
  "displayName": "Example VCS",
  "baselines": [
    { "id": "working", "label": "Working changes" },
    { "id": "branch", "label": "Changes since branch base" }
  ]
}
```

The repository root must contain the requested scope. A provider must return that
scope unchanged and limit status/content queries accordingly. Scope containment in
the protocol is lexical; filesystem symlinks require a separate check before Zed
opens a working file. Multiple windows may open different scopes in one repository.

### Snapshots and contents

```json
{
  "id": "snapshot:9",
  "repositoryId": "example:checkout-1",
  "baselineId": "working",
  "revisionId": "r42",
  "changes": [{ "path": "src/main.rs", "status": "modified", "kind": "file" }]
}
```

`revisionId` identifies the immutable contents of the selected baseline, which may
differ from the current checkout revision. Providers must change it whenever any
baseline-side content changes. Rename mapping changes must be reflected in the
corresponding change entry’s `oldPath`. A fresh snapshot token with the
same baseline, revision identity and change list does not invalidate already
loaded comparison contents. Old snapshot tokens must continue resolving their
pinned contents, or return an explicit expiration error.

Statuses are `added`, `modified`, `deleted`, `renamed`, or `untracked`. A rename
includes `oldPath`. Content kinds are `file`, `binary`, `symlink`, or `submodule`.
Optional `oldMode` and `newMode` are numeric Unix mode bits, e.g. decimal `33188`
for octal `100644`. A content-kind hint may be conservative; `readContent` provides
the authoritative baseline type.

A writable provider must include files with staged or unstaged changes even when
their combined comparison is empty, so that staged changes remain accessible.
The operation state is independent of the selected comparison baseline.

`readContent.path` identifies the current-side path. For a rename, the provider
resolves its original baseline path using the requested snapshot. It also accepts
unchanged files within the scope, so Zed can compare an unsaved buffer whose disk
file does not appear in status. The result is one of:

```json
{"kind":"text","text":"baseline contents\n"}
{"kind":"absent"}
{"kind":"binary"}
{"kind":"symlink","target":"relative-target"}
```

`absent` means the file is absent from the baseline, never that a backend read
failed. Comparison contents remain pinned to the requested snapshot. The snapshot
does not promise an atomic working filesystem: Zed owns live buffer versions and
rejects stale asynchronous results when a repository or baseline changes.

## Validation

Public conformance tests use a provider without Git metadata, an index, or Git SHA
identifiers. They cover framing, typed errors, cancellation, late responses,
response-before-EOF, stderr limits and child cleanup. Product tests additionally
cover trust, multiple scopes, nested Git ownership, unsaved buffers, native UI and
capability checks at dispatch. A successful CLI exchange alone does not establish
native UI correctness.
