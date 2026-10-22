---
title: External VCS Providers
description: Experimental external version control providers
---

# External VCS Providers

For the architecture, current Arc adapter coverage, staging behavior, and remaining
work, see the [external VCS and Arc overview (Russian)](./vcs-overview.ru.md).

This experimental API lets an extension supply repository discovery, changed files,
and comparison contents to Zed's native source-control UI. Git keeps its existing
backend. A provider does not emulate Git or create `.git` metadata.

The API is local. Protocol 1 is read-only; protocol 2 can explicitly enable
whole-file staging and unstaging, discarding unstaged changes, scoped commits, and
branch switching, and checkout catalog/create/open/prepare/suspend/remove operations.
Agent archive and remote transport are not provided.
Read-only capabilities constrain Zed's calls; they are not an operating-system
sandbox for the provider executable. Zed requires workspace trust before invoking
the extension callback or starting its process.

## Using external repositories {#using-external-repositories}

Open the [Git Panel](../git.md#git-panel) with
{#action git_panel::ToggleFocus}. Its repository picker, also available through
{#action git::SelectRepo}, lists both Git and external repositories. Select a
repository to see its changes and available actions. External repositories use
the same panel and keybindings; there is no separate Source Control dock.

Choose a provider's comparison from the dropdown beside **Stage All** or
**Unstage All** in the Changes tab, then click **View Diff** or run
{#action git::Diff}. The diff combines the selected repository's
changed text files in one editor. Working comparisons share live buffers with ordinary file
editors, preserving unsaved edits. Staged comparisons use a read-only snapshot of
the index; editing or saving that snapshot cannot change a working file. Navigating the changes list moves to the
selected file in the diff.

With the changes list focused, {#action git::StageFile},
{#action git::UnstageFile}, and {#action git::RestoreFile} use the selected file's
provider capabilities. The checkbox stages or unstages the whole file.
Use **Stage All** or **Unstage All**, also available as {#action git::StageAll}
and {#action git::UnstageAll}, to process the selected repository's scope. These
actions use fresh working changes even when you are viewing another comparison.
See [Stage All and Unstage All](#stage-all-and-unstage-all) for partial failures
and recovery.

Type a message in the standard commit editor, then choose **Commit** or run
{#action git::Commit} with the editor focused. {#action git::ExpandCommitEditor}
opens the standard commit modal. Drafts are kept separately for each repository.
The editor remains available when committing is unsupported; the disabled button
explains why. {#action git::Switch} opens the existing-branch switch controls when
supported.

Use **Discard Changes** from a file's context menu or the diff header's context
menu to discard that file's unstaged changes. The operation requires confirmation
and the provider's discard capability. Disabled actions explain why they are unavailable.

Providers that support blame use the existing {#action git::Blame},
{#action editor::ToggleGitBlameInline}, and {#action editor::BlameHover} commands.
The hover shows the author and commit details, with an external commit link when
the provider supplies one. Previous-revision blame remains Git-only.

The context menu includes file navigation, path copying, and the operations the
provider supports. **View Options** uses the same list/tree views, path/name
sorting, and grouping options as Git. File icons and status colors follow the
Git Panel settings. A checked box means staged, an empty box means unstaged, and
a mixed box means both. With **Staged & Unstaged** grouping, a partially staged
file appears in both sections. When the provider exposes index comparisons,
each row opens its corresponding comparison.
**Staged Changes** compares the committed version with the index; **Unstaged
Changes** compares the index with the working buffer. These views keep their own
comparison when the panel selection changes. Providers without index comparisons
do not offer these entries.

**View File History** opens a native, paginated history list when supported. Select
a revision to compare it with its parent in a read-only diff. A provider can bound
the available history; the view indicates truncation instead of presenting it as
complete. **Add to ignore file** uses the provider's declared configuration path
and requires confirmation before updating that file.

Repository-wide history, section and folder staging, hunk mutations, stash, amend,
signoff, hook options, AI commit-message generation, and remote Git operations are unavailable
for external repositories. Shared native controls do not enable capabilities the
provider does not expose. Unsupported commands report that limitation instead of
operating on another Git repository in the project. Use the provider's own tools
for operations it does not expose to Zed.

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

When migrating from a compatibility bridge that left synthetic Git metadata,
`git.excluded_repository_markers` can prevent that metadata from activating the
Git backend:

```json
{
  "git": { "excluded_repository_markers": [".example-compatibility-bridge"] }
}
```

The default is empty, so ordinary Git discovery is unchanged. Each entry is a
literal basename checked inside the repository's Git directory. For a `.git`
file, Zed resolves its `gitdir:` target and checks that directory, without inheriting
markers from the shared `commondir`. Invalid names are logged and ignored.
This setting excludes only matching Git metadata; it does not select or start a
provider. The extension must still declare its root marker, confirm ownership,
and pass workspace trust. A nested Git repository without an exclusion marker
keeps ownership of its files. Exclusion also blocks Git initialization and mutation
when a provider is missing, disabled, or fails, so recovery cannot silently switch
backends. Use this only with a marker that positively identifies bridge metadata;
do not use ordinary Git files such as `HEAD` or `config` as exclusion markers.

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

| Method             | Parameters                                                                    | Result                                                |
| ------------------ | ----------------------------------------------------------------------------- | ----------------------------------------------------- |
| `initialize`       | `protocolVersion`, `supportedProtocolVersions`, `clientInfo: {name, version}` | `protocolVersion`, `serverInfo`, `capabilities`       |
| `vcs/discover`     | `scopeUri`                                                                    | Array of repository descriptors                       |
| `vcs/snapshot`     | `repositoryId`, `baselineId`                                                  | A snapshot of changes for that comparison             |
| `vcs/readContent`  | `repositoryId`, `snapshotId`, `baselineId`, `path`, optional `side`           | Immutable base or current-side content                |
| `vcs/blame`        | `repositoryId`, `path`                                                        | Committed content and line attribution when supported |
| `vcs/fileHistory`  | `repositoryId`, `path`, optional `cursor`, `limit`                            | A bounded page of file revisions                      |
| `vcs/readRevision` | `repositoryId`, `path`, `revisionId`                                          | Immutable revision content                            |
| `shutdown`         | Empty object                                                                  | `null`, after provider cleanup                        |

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

### Index comparisons and file history {#provider-file-workflow}

A baseline can declare `purpose` as `working`, `staged`, `unstaged`, or `branch`.
Its `currentContent` defaults to `working`. Use `snapshot` for an immutable
current side, such as the index in a staged comparison. Such snapshots require
`currentRevisionId` in addition to the baseline's `revisionId`; both identities
must change whenever their respective content changes. A working-side comparison
against the index uses the index identity as `revisionId`. `headRevisionId` still
identifies the committed revision for blame.

`vcs/readContent` accepts `side: "base"` (the default) or `"current"`. Both sides
must be frozen to the requested snapshot, including after the live index changes.
Zed never attaches snapshot-backed current content to a writable filesystem file.
Changes may include `stagedStatus` and `unstagedStatus` to describe index and
working changes independently, including changes that cancel each other.

Set `capabilities.fileHistory` to enable file history. A request's `limit` is
between 1 and 100. The response contains `entries`, optional `nextCursor`, and
`truncated` (default false). Each entry contains `commit` using the blame commit
schema, optional `parentRevisionId`, and optional scope-relative `path` and
`parentPath` for renames. A missing parent revision means the empty base for the
file's initial revision. Cursors must preserve a consistent history session.
`truncated: true` means older entries may exist outside the provider's bound.

`vcs/readRevision` returns the usual `Content` at an immutable revision. A file
absent at that revision returns `kind: "absent"`; actual command failures remain
errors. The host validates ownership and rejects stale responses after discovery
or provider connection changes.

### Line attribution {#provider-blame}

Set `capabilities.blame` to `true` to support `vcs/blame`. This optional read-only
capability is independent of mutation permissions and works with protocol 1 or 2.
A supporting provider must include `headRevisionId` in each snapshot: it identifies
the current committed revision even when `revisionId` identifies a different
comparison baseline.

Return attribution for the exact committed content at one pinned revision:

```json
{
  "revisionId": "revision:42",
  "content": "first line\n",
  "entries": [{ "startLine": 0, "endLine": 1, "commitId": "change:7" }],
  "commits": [
    {
      "revisionId": "change:7",
      "author": "Example Author",
      "authorTime": 1700000000,
      "summary": "Add first line",
      "message": "Add first line",
      "commitUrl": "https://example.test/changes/7"
    }
  ]
}
```

Line ranges are zero-based, half-open, contiguous, and cover all committed lines,
excluding the empty row after a final newline. Each `commitId` references exactly
one commit record. Revision IDs remain opaque strings. Commit fields other than
`revisionId` are optional; `authorTime` is Unix time in seconds and `commitUrl`
must use HTTPS without embedded credentials. The host accepts at most 4 MiB of
text and 100,000 lines; binary content is unsupported.

Zed maps committed attribution to the live editor buffer, leaving edited lines
unattributed. Results are cached by file and HEAD, and discarded when ownership,
HEAD, or the provider connection changes. Cursor movement uses the existing
in-memory attribution.

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
    "fileOperations": ["stage", "unstage", "discard", "ignore"]
  }
}
```

Missing `fileOperations` means no file operations. Setting `readOnly: false`
does not enable any operation by itself. A read-only provider must not advertise
file operations; contradictory capabilities are rejected.

### Selected-file operations

The Git Panel uses the file checkbox for whole-file staging and unstaging.
The file context menu includes Stage File, Unstage File, and Discard for the
selected change. The optional `fileOperations` on each change narrows provider-level capabilities;
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
requested path and may additionally include its known rename source. For `ignore`,
the repository must declare `ignoreConfigPath`, and `affectedPaths` must contain
exactly the selected untracked path and that configuration path, with a nonempty
summary. The provider preserves unrelated rules and file contents; changes to the
selection or configuration after preparation must invalidate the token. The host validates
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

### Stage All and Unstage All {#stage-all-and-unstage-all}

Stage All and Unstage All use the provider's `stage` and `unstage` file
capabilities. Zed fixes a list of unique paths from the loaded Working comparison
and pending staging intents within the selected repository scope. It loads that
comparison first only when it is unavailable. Partially staged files can
participate in either action. A rename and its source are handled together.

Like Git staging, a checkbox immediately reflects the latest requested state
while index writes run in a serialized queue. Other staging controls remain
available; adjacent queued requests in the same direction can share one write.
Completed pending state remains visible until a fresh snapshot arrives, so an
older response cannot undo a later click. Other mutations stay blocked while
staging is in progress.

Before the first write, Zed checks every candidate's capabilities, ownership,
file metadata, and open buffers. An unsupported candidate, unsaved edit, or disk
conflict rejects the batch before any file is modified.

A protocol 2 provider can opt into one operation for the entire selection with
`batchFileOperations: ["stage", "unstage"]`. This capability defaults to an empty
list and requires `readOnly: false` and the corresponding entries in
`fileOperations`. It does not grant commit or branch permissions. Only `stage`
and `unstage` are allowed.

| Method                   | Params                               | Result                                      |
| ------------------------ | ------------------------------------ | ------------------------------------------- |
| `vcs/prepareFileBatch`   | `repositoryId`, `paths`, `operation` | `{operationToken, affectedPaths, summary?}` |
| `vcs/applyFileBatch`     | Same fields plus `operationToken`    | `{outcome}`                                 |
| `vcs/reconcileFileBatch` | Same fields plus `operationToken`    | `{outcome}`                                 |

`paths` contains 1–1024 unique, scope-relative selected paths. The provider must
return every selected path and any rename partner in `affectedPaths`; Zed rejects
paths outside that exact set of permitted targets. Preparation is read-only and
binds one durable token to the repository, scope, complete path list, operation,
and provider-defined preconditions. Stage and Unstage can use current file
contents at execution time, without reading file blobs during preparation.
Apply must revalidate the checkout and every target before writing, preserve
working files, and verify the resulting index state. The provider should execute
one native staging command for the selection.

Zed journals one token before dispatching a batch. `applied` confirms the entire
batch, `notApplied` confirms that none of it was applied, and `conflict` means the
observed state no longer matches the prepared operation, including a partial
result. An apply error is ambiguous: Zed reconciles the same complete request and
token, never retries apply or switches to individual file operations. A failed
reconciliation leaves the journal entry unresolved across restarts. The existing
token-consumption and recovery rules apply to the whole batch. An unknown
outcome stops the queue until reconciliation. A known failure removes the failed
pending state and remains visible; independently queued user requests can still
run after their own validation.

Providers without this capability retain the sequential selected-file workflow.
They receive only the existing singular RPCs; bulk paths are never added to a
singular request. For this fallback, Zed prepares and applies each file using the
same durable journal and reconciliation rules as selected-file operations.

The sequential fallback stops at the first error. The error reports how many file operations
were confirmed, the current path when one was started, and how many were not
started. Confirmed operations remain applied; Zed does not automatically roll
them back or resume the remaining batch. Recovery reconciles individual recorded
operations only. Review the refreshed changes before starting a new batch.

Section, folder, and hunk staging remain unavailable. Use individual file actions
or Stage All and Unstage All for the repository scope.

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
remain on disk. Choosing **Commit** in the standard editor prepares and applies
this scoped operation without a second confirmation dialog. Zed rechecks
ownership, open buffers, and the prepared state before applying it.

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

The native Checkouts picker is available from the Git Panel's changes actions
menu, the title bar, and `vcs::OpenCheckouts`. It requires a live provider repository as its catalog anchor.
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
