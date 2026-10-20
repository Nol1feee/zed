#!/usr/bin/env python3
"""Neutral stdio fixture; default protocol 1 needs no VCS or repository metadata.

Usage: python3 crates/vcs/tests/fixture_provider.py --scenario normal --events FILE
Feed Content-Length framed JSON-RPC. `--events` records test-only lifecycle events.
Scenarios: normal, reply-eof, crash, error, pending, malformed, oversized,
stderr-flood, descendant, early-exit, reordered, hang-after-exit,
exit-leaves-descendant, polling-content, changing-slow-content, pending-refresh.
`reordered` holds the first
snapshot, then sends the second response before the first, even after cancellation.
`descendant` creates one idle child in the
same process group so the host's forced-close ownership can be tested.
`hang-after-exit` acknowledges shutdown but hangs after recording exit receipt.
`exit-leaves-descendant` exits normally while its child keeps stdout open.
`polling-content` creates fresh snapshot IDs; FILE.revision containing r43 moves
later snapshots from r42 to r43 while older snapshot contents remain pinned.
`changing-slow-content` changes each snapshot revision and leaves content requests
pending while still processing snapshots, cancellation, and shutdown.
`pending-refresh` replies to the first snapshot, then holds later snapshots and
records refresh-held without blocking cancellation or shutdown.
`writable` negotiates protocol 2 and maintains synthetic per-file staged state.
Changing FILE.state invalidates prepared tokens. Every terminal result consumes
its token; repeated apply is rejected and reconciliation retains the saved outcome.
`write-error` and `write-invalid-result` apply once before sending an ambiguous
response, so callers must reconcile the token.
`write-unknown` also rejects reconciliation until FILE.reconcile exists.
`write-reconcile-crash` applies before returning an error and exits on every reconcile.
`prepare-pending` holds preparation until FILE.release exists and remains
responsive to cancellation and shutdown while waiting.
`discard-pending` records its disk effect, then waits for FILE.release before replying.
`persistent-write-unknown` persists synthetic state across fixture restarts and
rejects reconcile until FILE.reconcile exists.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time

MAX_MESSAGE_BYTES = 16 * 1024 * 1024
EOF_TEXT = "baseline — r42\n" * 8192
SCENARIOS = ("normal", "reply-eof", "crash", "error", "pending", "malformed",
             "oversized", "stderr-flood", "descendant", "early-exit", "reordered",
             "hang-after-exit", "exit-leaves-descendant", "polling-content",
             "changing-slow-content", "pending-refresh",
             "writable", "write-error", "write-invalid-result", "write-unknown", "write-reconcile-crash", "prepare-pending",
             "v2-read-only", "v2-no-file-operations", "v2-stage-only",
             "v2-contradictory", "v1-writable", "invalid-token",
             "discard-pending", "persistent-write-unknown",
             "repository-operations", "repository-clean", "repository-unknown",
             "checkouts", "checkout-read-only", "checkout-unknown", "checkout-pending",
             "checkout-preparation-failed", "checkout-invalid-result")
WRITABLE_SCENARIOS = {"writable", "write-error", "write-invalid-result", "write-unknown", "write-reconcile-crash",
                      "prepare-pending", "v2-stage-only", "invalid-token",
                      "discard-pending", "persistent-write-unknown",
                      "repository-operations", "repository-clean", "repository-unknown",
                      "checkouts", "checkout-unknown", "checkout-pending",
                      "checkout-preparation-failed", "checkout-invalid-result"}
REPOSITORY_SCENARIOS = {"repository-operations", "repository-clean", "repository-unknown"}
CHECKOUT_SCENARIOS = {"checkouts", "checkout-read-only", "checkout-unknown", "checkout-pending",
                      "checkout-preparation-failed", "checkout-invalid-result"}
FILE_KIND_SCENARIOS = {"file-kinds", "file-kinds-missing-summary", "file-kinds-bad-targets", "file-kinds-missing-old-path",
                       "file-kinds-old-parent", "file-kinds-old-directory"}
SCENARIOS += tuple(sorted(FILE_KIND_SCENARIOS))
WRITABLE_SCENARIOS.update(FILE_KIND_SCENARIOS)
SEND_LOCK = threading.Lock()


def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, _, value = line.partition(b":")
        if key.lower() == b"content-length":
            length = int(value)
    if length is None or not 0 <= length <= MAX_MESSAGE_BYTES:
        raise ValueError("invalid fixture request length")
    body = sys.stdin.buffer.read(length)
    if len(body) != length:
        raise ValueError("truncated fixture request")
    return json.loads(body)


def send(message):
    body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode()
    with SEND_LOCK:
        sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        sys.stdout.buffer.flush()


def reply(identifier, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": identifier}
    message.update({"error": error} if error is not None else {"result": result})
    send(message)


def changed():
    send({"jsonrpc": "2.0", "method": "vcs/didChange", "params": {"repositoryId": "fixture-repository"}})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", choices=SCENARIOS, default="normal")
    parser.add_argument("--events", type=Path)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--ready-fd", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.child:
        os.write(args.ready_fd, b"ready\n")
        os.close(args.ready_fd)
        while True:
            signal.pause()

    def record(event, **details):
        if args.events:
            with args.events.open("a", encoding="utf-8") as output:
                output.write(json.dumps({"event": event, **details}) + "\n")

    record("provider-started", pid=os.getpid())
    if args.scenario == "early-exit":
        return 17
    descendant = None
    if args.scenario in ("descendant", "hang-after-exit", "exit-leaves-descendant"):
        read_ready, write_ready = os.pipe()
        try:
            descendant = subprocess.Popen([sys.executable, __file__, "--child", "--ready-fd", str(write_ready)],
                                          stdin=subprocess.DEVNULL, pass_fds=(write_ready,))
        finally:
            os.close(write_ready)
        with os.fdopen(read_ready, "rb") as ready:
            assert ready.readline() == b"ready\n"
        record("descendant-started", pid=descendant.pid)
    snapshots = 0
    held_snapshot = None
    snapshot_revisions = {}
    snapshot_contents = {}
    file_states = {}
    operations = {}
    apply_count = 0
    repository_operations = {}
    repository_branch = "main"
    repository_head = "head"
    repository_commits = 0
    directory = args.events.parent if args.events else Path.cwd()
    checkout_base = directory.parent / (directory.name + "-fixture-checkouts")
    checkout_entries, checkout_operations, checkout_count = {}, {}, 0
    saved_checkouts = Path(str(args.events) + ".checkouts.json") if args.events else None

    def checkout_descriptor(identifier, name, state, owned):
        root = checkout_base / name
        operations = [] if not owned or state == "missing" else ["open", "prepare"]
        if owned and state == "mounted":
            operations += ["suspend", "remove"]
        descriptor = {"id": identifier, "name": name, "rootUri": root.as_uri(), "scopeUri": (root / "project").as_uri(),
                      "branch": "main", "state": state, "owned": owned, "operations": operations, "preparation": "ready"}
        if not owned or state in ("missing", "busy"):
            descriptor["unavailableReason"] = "Fixture checkout is unavailable for destructive operations"
        return descriptor

    if args.scenario in CHECKOUT_SCENARIOS:
        if saved_checkouts and saved_checkouts.is_file():
            saved = json.loads(saved_checkouts.read_text())
            checkout_entries, checkout_operations, checkout_count = saved["entries"], saved["operations"], saved["count"]
        else:
            for identifier, name, state, owned in [
                ("fixture-checkout", "managed", "mounted", True),
                ("unmounted-checkout", "unmounted", "unmounted", True),
                ("missing-checkout", "missing", "missing", False),
                ("foreign-checkout", "foreign", "mounted", False),
                ("busy-checkout", "busy", "busy", True),
            ]:
                checkout_entries[identifier] = checkout_descriptor(identifier, name, state, owned)

    def persist_checkouts():
        if saved_checkouts:
            temporary = saved_checkouts.with_suffix(".tmp")
            temporary.write_text(json.dumps({"entries": checkout_entries, "operations": checkout_operations, "count": checkout_count}))
            temporary.replace(saved_checkouts)

    def checkout_fingerprint():
        marker = Path(str(args.events) + ".checkout-state") if args.events else None
        return {"entries": json.loads(json.dumps(checkout_entries)),
                "marker": marker.read_text() if marker and marker.is_file() else ""}

    def send_progress(token, phase, message):
        send({"jsonrpc": "2.0", "method": "vcs/operationProgress",
              "params": {"repositoryId": "fixture-repository", "operationToken": token, "phase": phase, "message": message}})
    if args.scenario in REPOSITORY_SCENARIOS:
        file_states["src/example.txt"] = ("head", "head") if args.scenario == "repository-clean" else ("staged", "working")
    saved_operations = Path(str(args.events) + ".operations.json") if args.events else None
    if args.scenario in ("persistent-write-unknown", "repository-unknown") and saved_operations and saved_operations.is_file():
        saved = json.loads(saved_operations.read_text())
        file_states, operations, apply_count = saved["files"], saved["operations"], saved["count"]
        repository_operations = saved.get("repositoryOperations", {})
        repository_branch, repository_head, repository_commits = saved.get("repository", ["main", "head", 0])

    def persist_operations():
        if args.scenario in ("persistent-write-unknown", "repository-unknown") and saved_operations:
            temporary = saved_operations.with_suffix(".tmp")
            temporary.write_text(json.dumps({"files": file_states, "operations": operations, "count": apply_count,
                                             "repositoryOperations": repository_operations,
                                             "repository": [repository_branch, repository_head, repository_commits]}))
            temporary.replace(saved_operations)

    def file_state(path):
        marker = Path(str(args.events) + ".state") if args.events else None
        revision = marker.read_text() if marker and marker.is_file() else ""
        index, disk = file_states.get(path, ("head", "working"))
        return [revision, index, disk]

    def repository_fingerprint():
        return {"branch": repository_branch, "head": repository_head,
                "commits": repository_commits, "files": {path: file_state(path) for path in file_states}}

    def release_reply(identifier, result, event):
        marker = Path(str(args.events) + ".release") if args.events else None
        while marker is None or not marker.is_file():
            time.sleep(0.01)
        record(event, id=identifier)
        reply(identifier, result)

    def file_kind_changes():
        changes = [{"path": "src/example.txt", "status": "modified", "kind": "file", "oldMode": 33188, "newMode": 33188}]
        for path, status, kind in [
            ("new.txt", "untracked", "file"), ("deleted.txt", "deleted", "file"),
            ("staged-missing.txt", "added", "file"),
            ("binary.bin", "modified", "binary"), ("link", "modified", "symlink"),
            ("renamed.txt", "renamed", "file"), ("type-change.txt", "modified", "file"),
            ("executable.sh", "modified", "file"), ("directory", "modified", "file"),
            ("linked-parent/child.txt", "modified", "file"), ("disabled.txt", "modified", "file"),
            ("binary-no-capabilities.bin", "modified", "binary"),
        ]:
            change = {"path": path, "status": status, "kind": kind, "oldMode": 33188, "newMode": 33188}
            if path != "binary-no-capabilities.bin":
                change["fileOperations"] = ["stage", "unstage", "discard"]
            if path == "disabled.txt":
                change.update(fileOperations=[], unavailableReason="Fixture explicitly disables this row")
            if status == "renamed":
                change["oldPath"] = ("linked-parent/old.txt" if args.scenario == "file-kinds-old-parent"
                                     else "directory" if args.scenario == "file-kinds-old-directory" else "old.txt")
            if kind == "symlink":
                change.update(oldMode=40960, newMode=40960)
            if path == "type-change.txt":
                change["oldMode"] = 40960
            if path == "executable.sh":
                change["newMode"] = 33261
            changes.append(change)
        return changes

    while (message := read_message()) is not None:
        method, identifier = message.get("method"), message.get("id")
        params = message.get("params") or {}
        record("message", method=method, id=identifier)
        if method == "exit":
            record("exit-received")
            if args.scenario == "hang-after-exit":
                while True:
                    signal.pause()
            if args.scenario == "exit-leaves-descendant":
                os._exit(0)
            if descendant is not None:
                descendant.terminate()
                descendant.wait(timeout=3)
            return 0
        if method == "$/cancelRequest":
            record("cancel-received", id=params["id"])
            if args.scenario not in ("reordered", "changing-slow-content", "pending-refresh"):
                changed()
            continue
        if method == "initialize":
            if args.scenario == "stderr-flood":
                sys.stderr.buffer.write(b"discarded-prefix\n" + ("diagnostic ✓\n" * 40000).encode() + b"fixture-stderr-tail\n")
                sys.stderr.buffer.flush()
            record("initialize-received", protocol_version=params.get("protocolVersion"),
                   supported_protocol_versions=params.get("supportedProtocolVersions"))
            version, capabilities = 1, {"readOnly": True}
            if args.scenario in WRITABLE_SCENARIOS or args.scenario.startswith("v2-"):
                version = 2
                capabilities = {"readOnly": False, "fileOperations": ["stage", "unstage", "discard"]}
                if args.scenario == "v2-read-only":
                    capabilities = {"readOnly": True}
                elif args.scenario == "v2-no-file-operations":
                    capabilities = {"readOnly": False}
                elif args.scenario == "v2-stage-only":
                    capabilities["fileOperations"] = ["stage"]
                elif args.scenario == "v2-contradictory":
                    capabilities["readOnly"] = True
            elif args.scenario == "v1-writable":
                capabilities = {"readOnly": False, "fileOperations": ["stage"]}
            if args.scenario in REPOSITORY_SCENARIOS:
                capabilities["repositoryOperations"] = ["commit", "switchBranch"]
            if args.scenario in CHECKOUT_SCENARIOS:
                version = 2
                if args.scenario == "checkout-read-only":
                    capabilities = {"readOnly": True, "checkoutCatalog": True}
                else:
                    capabilities.update(checkoutCatalog=True, checkoutOperations=["create", "open", "prepare", "suspend", "remove"])
            reply(identifier, {"protocolVersion": version, "serverInfo": {"name": "fixture", "version": "1"}, "capabilities": capabilities})
        elif method == "vcs/discover":
            reply(identifier, [{"id": "fixture-repository", "rootUri": params["scopeUri"], "scopeUri": params["scopeUri"],
                                "displayName": "Fixture", "baselines": [{"id": "submitted", "label": "Submitted changes"},
                                                                          {"id": "previous", "label": "Previous changes"}]}])
        elif method == "vcs/snapshot":
            snapshots += 1
            if args.scenario == "pending-refresh" and snapshots > 1:
                record("refresh-held", id=identifier)
                continue
            if args.scenario == "crash":
                if snapshots == 2:
                    sys.stderr.write("fixture crash\n")
                    sys.stderr.flush()
                    return 19
                continue
            if args.scenario == "pending":
                record("request-pending", id=identifier)
                changed()
                continue
            if args.scenario == "error":
                reply(identifier, error={"code": -32044, "message": "Snapshot needs refresh",
                                          "data": {"kind": "stale", "revision": "r42", "retryable": True}})
                continue
            if args.scenario in ("malformed", "oversized"):
                header = b"Content-Length: invalid\r\n\r\n" if args.scenario == "malformed" else f"Content-Length: {MAX_MESSAGE_BYTES + 1}\r\n\r\n".encode()
                sys.stdout.buffer.write(header)
                sys.stdout.buffer.flush()
                continue
            previous = params["baselineId"] == "previous"
            snapshot = {"id": "snapshot:previous" if previous else "snapshot:7", "repositoryId": params["repositoryId"], "baselineId": params["baselineId"],
                        "revisionId": "r41" if previous else "r42", "changes": [{"path": "src/example.txt", "status": "modified", "kind": "file", "oldMode": 33188, "newMode": 33188}]}
            if args.scenario in REPOSITORY_SCENARIOS:
                snapshot["id"] = f"snapshot:repository:{snapshots}"
                snapshot["revisionId"] = f"{repository_branch}:{repository_commits}:{repository_head}"
                snapshot_contents[snapshot["id"]] = repository_head + "\n"
                if all(index == repository_head and disk == repository_head for index, disk in file_states.values()):
                    snapshot["changes"] = []
            if args.scenario in FILE_KIND_SCENARIOS:
                snapshot["changes"] = file_kind_changes()
            if args.scenario == "polling-content":
                marker = Path(str(args.events) + ".revision") if args.events else None
                revision = "r43" if marker and marker.is_file() and marker.read_text().strip() == "r43" else "r42"
                snapshot["id"] = f"snapshot:polling:{snapshots}"
                snapshot["revisionId"] = revision
                snapshot_revisions[snapshot["id"]] = revision
            if args.scenario == "changing-slow-content":
                snapshot["id"] = f"snapshot:changing:{snapshots}"
                snapshot["revisionId"] = f"r{42 + snapshots}"
            if args.scenario == "reordered" and snapshots == 1:
                held_snapshot = (identifier, snapshot)
                record("snapshot-held", id=identifier)
                continue
            reply(identifier, snapshot)
            if args.scenario == "reordered" and held_snapshot is not None:
                reply(*held_snapshot)
                held_snapshot = None
        elif method == "vcs/readContent":
            if args.scenario == "changing-slow-content":
                record("content-held", id=identifier, snapshot_id=params["snapshotId"])
                continue
            path = params["path"]
            text = EOF_TEXT if args.scenario == "reply-eof" else "previous — r41\n" if params["baselineId"] == "previous" else "baseline — r42\n"
            if args.scenario in REPOSITORY_SCENARIOS:
                text = snapshot_contents.get(params["snapshotId"], repository_head + "\n")
            if args.scenario == "polling-content":
                revision = snapshot_revisions.get(params.get("snapshotId"))
                if revision is None:
                    reply(identifier, error={"code": -32013, "message": "Unknown fixture snapshot", "data": {"kind": "snapshotExpired"}})
                    continue
                text = f"baseline — {revision}\n"
            content = ({"kind": "absent"} if path == "absent.txt" else
                       {"kind": "binary"} if path == "binary.bin" else
                       {"kind": "symlink", "target": "../target"} if path == "link" else
                       {"kind": "text", "text": text})
            reply(identifier, content)
            if args.scenario == "reply-eof":
                return 0
        elif method in ("vcs/prepareFileOperation", "vcs/applyFileOperation",
                        "vcs/reconcileFileOperation"):
            if args.scenario not in WRITABLE_SCENARIOS:
                reply(identifier, error={"code": -32601, "message": "File operations unavailable"})
                continue
            path, operation = params.get("path"), params.get("operation")
            if (params.get("repositoryId") != "fixture-repository" or not isinstance(path, str)
                    or not path or operation not in ("stage", "unstage", "discard")
                    or args.scenario == "v2-stage-only" and operation != "stage"):
                reply(identifier, error={"code": -32602, "message": "Invalid file operation"})
                continue
            if method == "vcs/prepareFileOperation":
                token = f"fixture:operation:{len(operations) + 1}"
                operations[token] = {"path": path, "operation": operation,
                                     "before": file_state(path), "after": None, "terminal": None}
                persist_operations()
                if args.scenario == "prepare-pending":
                    record("prepare-held", id=identifier, path=path)
                    threading.Thread(target=release_reply,
                                     args=(identifier, {"operationToken": token}, "prepare-released"), daemon=True).start()
                else:
                    prepared_result = {"operationToken": "" if args.scenario == "invalid-token" else token}
                    if args.scenario in FILE_KIND_SCENARIOS:
                        affected = [path]
                        if path == "renamed.txt" and args.scenario != "file-kinds-missing-old-path":
                            affected.append("old.txt")
                        if args.scenario == "file-kinds-bad-targets":
                            affected.append("neighbor.txt")
                        prepared_result["affectedPaths"] = affected
                        if args.scenario != "file-kinds-missing-summary":
                            effect = "Permanently delete" if operation == "discard" and path == "new.txt" else operation.capitalize()
                            prepared_result["summary"] = effect + ": " + ", ".join(affected)
                    reply(identifier, prepared_result)
                continue
            prepared = operations.get(params.get("operationToken"))
            state = file_state(path)
            if method == "vcs/reconcileFileOperation" and args.scenario == "write-reconcile-crash":
                record("reconcile-crashed", id=identifier)
                return 23
            if method == "vcs/reconcileFileOperation" and args.scenario in ("write-unknown", "persistent-write-unknown"):
                marker = Path(str(args.events) + ".reconcile") if args.events else None
                if marker is None or not marker.is_file():
                    reply(identifier, error={"code": -32046, "message": "Reconciliation unavailable",
                                             "data": {"kind": "outcomeUnknown"}})
                    continue
            if (prepared is None or prepared["path"] != path
                    or prepared["operation"] != operation):
                reply(identifier, error={"code": -32018, "message": "Unknown or mismatched file token",
                                         "data": {"kind": "outcomeUnknown"}})
                continue
            if prepared.get("terminal") is not None:
                if method == "vcs/applyFileOperation":
                    reply(identifier, error={"code": -32049, "message": "Token consumed; prepare a new operation",
                                             "data": {"kind": "tokenConsumed"}})
                else:
                    reply(identifier, {"outcome": prepared["terminal"]})
                continue
            if prepared["after"] == state:
                outcome = "applied"
            elif prepared["after"] is not None or prepared["before"] != state:
                outcome = "conflict"
            elif method == "vcs/reconcileFileOperation":
                outcome = "notApplied"
            else:
                if operation == "stage":
                    file_states[path] = (state[2], state[2])
                elif operation == "unstage":
                    file_states[path] = (repository_head if args.scenario in REPOSITORY_SCENARIOS else "head", state[2])
                else:
                    file_states[path] = (state[1], state[1])
                prepared["after"] = file_state(path)
                prepared["terminal"] = "applied"
                apply_count += 1
                persist_operations()
                record("file-operation-applied", path=path, operation=operation,
                       count=apply_count, staged=file_states[path][0] != "head",
                       disk=file_states[path][1])
                if args.scenario == "discard-pending" and operation == "discard":
                    record("apply-held", id=identifier, path=path)
                    threading.Thread(target=release_reply,
                                     args=(identifier, {"outcome": "applied"}, "apply-released"), daemon=True).start()
                    continue
                if args.scenario in ("write-error", "write-unknown", "write-reconcile-crash", "persistent-write-unknown"):
                    reply(identifier, error={"code": -32045, "message": "Apply acknowledgement lost",
                                             "data": {"kind": "outcomeUnknown"}})
                    continue
                if args.scenario == "write-invalid-result":
                    reply(identifier, {"outcome": "unknown"})
                    continue
                outcome = "applied"
            if prepared.get("terminal") is None:
                prepared["terminal"] = outcome
                persist_operations()
            reply(identifier, {"outcome": outcome})
        elif method == "vcs/repositoryState":
            if args.scenario not in REPOSITORY_SCENARIOS:
                reply(identifier, error={"code": -32601, "message": "Repository state unavailable"})
                continue
            reply(identifier, {"currentBranch": {"id": repository_branch, "name": repository_branch},
                               "branches": [{"id": "main", "name": "main"}, {"id": "topic", "name": "topic"}],
                               "stagedPaths": [path for path, (index, _) in file_states.items() if index != repository_head]})
        elif method in ("vcs/prepareRepositoryOperation", "vcs/applyRepositoryOperation", "vcs/reconcileRepositoryOperation"):
            if args.scenario not in REPOSITORY_SCENARIOS:
                reply(identifier, error={"code": -32601, "message": "Repository operations unavailable"})
                continue
            request = {key: value for key, value in params.items() if key != "operationToken"}
            operation = request.get("operation")
            if request.get("repositoryId") != "fixture-repository" or operation not in ("commit", "switchBranch"):
                reply(identifier, error={"code": -32602, "message": "Invalid repository operation"})
                continue
            if method == "vcs/prepareRepositoryOperation":
                staged = [path for path, (index, _) in file_states.items() if index != repository_head]
                if operation == "commit" and (not staged or not request.get("message", "").strip()):
                    reply(identifier, error={"code": -32048, "message": "No staged files or commit message"})
                    continue
                if operation == "switchBranch" and (request.get("branchId") not in ("main", "topic") or
                        any(index != repository_head or disk != repository_head for index, disk in file_states.values())):
                    reply(identifier, error={"code": -32048, "message": "Checkout must be clean before switching"})
                    continue
                token = f"fixture:repository-operation:{len(repository_operations) + 1}"
                repository_operations[token] = {"request": request, "before": repository_fingerprint(), "after": None, "terminal": None}
                persist_operations()
                reply(identifier, {"operationToken": token, "summary": f"{operation} in fixture-repository",
                                   "affectedPaths": staged if operation == "commit" else []})
                continue
            prepared = repository_operations.get(params.get("operationToken"))
            if method == "vcs/reconcileRepositoryOperation" and args.scenario == "repository-unknown":
                marker = Path(str(args.events) + ".reconcile") if args.events else None
                if marker is None or not marker.is_file():
                    reply(identifier, error={"code": -32046, "message": "Reconciliation unavailable"})
                    continue
            state = repository_fingerprint()
            if prepared is None or prepared["request"] != request:
                reply(identifier, error={"code": -32018, "message": "Unknown or mismatched repository token",
                                         "data": {"kind": "outcomeUnknown"}})
                continue
            if prepared.get("terminal") is not None:
                if method == "vcs/applyRepositoryOperation":
                    reply(identifier, error={"code": -32049, "message": "Token consumed; prepare a new operation",
                                             "data": {"kind": "tokenConsumed"}})
                else:
                    reply(identifier, {"outcome": prepared["terminal"]})
                continue
            if prepared["after"] == state:
                outcome = "applied"
            elif prepared["after"] is not None or prepared["before"] != state:
                outcome = "conflict"
            elif method == "vcs/reconcileRepositoryOperation":
                outcome = "notApplied"
            else:
                if operation == "commit":
                    repository_head = file_states["src/example.txt"][0]
                    repository_commits += 1
                else:
                    repository_branch = request["branchId"]
                    repository_head = "topic" if repository_branch == "topic" else "head"
                    file_states["src/example.txt"] = (repository_head, repository_head)
                prepared["after"] = repository_fingerprint()
                prepared["terminal"] = "applied"
                persist_operations()
                record("repository-operation-applied", operation=operation, branch=repository_branch, commits=repository_commits)
                if args.scenario == "repository-unknown":
                    reply(identifier, error={"code": -32045, "message": "Repository acknowledgement lost"})
                    continue
                outcome = "applied"
            if prepared.get("terminal") is None:
                prepared["terminal"] = outcome
                persist_operations()
            reply(identifier, {"outcome": outcome})
        elif method == "vcs/listCheckouts":
            if args.scenario not in CHECKOUT_SCENARIOS:
                reply(identifier, error={"code": -32601, "message": "Checkout catalog unavailable"})
                continue
            reply(identifier, {"checkouts": list(checkout_entries.values())})
        elif method in ("vcs/prepareCheckoutOperation", "vcs/applyCheckoutOperation", "vcs/reconcileCheckoutOperation"):
            if args.scenario not in CHECKOUT_SCENARIOS or args.scenario == "checkout-read-only":
                reply(identifier, error={"code": -32601, "message": "Checkout operations unavailable"})
                continue
            request = {key: value for key, value in params.items() if key != "operationToken"}
            operation = request.get("operation")
            if request.get("repositoryId") != "fixture-repository" or operation not in ("create", "open", "prepare", "suspend", "remove"):
                reply(identifier, error={"code": -32602, "message": "Invalid checkout operation"})
                continue
            if method == "vcs/prepareCheckoutOperation":
                checkout = checkout_entries.get(request.get("checkoutId"))
                if operation == "create":
                    name = request.get("name", "")
                    if not name or any(item["name"] == name for item in checkout_entries.values()):
                        reply(identifier, error={"code": -32048, "message": "Checkout name exists or is invalid"})
                        continue
                    target = (checkout_base / name).as_uri()
                elif checkout is None or not checkout["owned"] or operation not in checkout["operations"]:
                    reply(identifier, error={"code": -32048, "message": "Checkout is unavailable"})
                    continue
                else:
                    target = checkout["rootUri"]
                token = f"fixture:checkout-operation:{len(checkout_operations) + 1}"
                checkout_operations[token] = {"request": request, "before": checkout_fingerprint(),
                                              "target": target, "result": None}
                persist_checkouts()
                prepared = {"operationToken": token, "summary": f"{operation} checkout at {target}", "targetRootUri": target}
                if checkout is not None:
                    prepared["checkout"] = checkout
                reply(identifier, prepared)
                continue
            token = params.get("operationToken")
            prepared = checkout_operations.get(token)
            if prepared is None or prepared["request"] != request:
                reply(identifier, error={"code": -32018, "message": "Unknown checkout token", "data": {"kind": "outcomeUnknown"}})
                continue
            if method == "vcs/reconcileCheckoutOperation" and args.scenario == "checkout-unknown":
                marker = Path(str(args.events) + ".reconcile") if args.events else None
                if marker is None or not marker.is_file():
                    reply(identifier, error={"code": -32018, "message": "Checkout outcome unknown", "data": {"kind": "outcomeUnknown"}})
                    continue
            if prepared["result"] is not None:
                if method == "vcs/applyCheckoutOperation":
                    reply(identifier, error={"code": -32049, "message": "Token consumed; prepare a new operation",
                                             "data": {"kind": "tokenConsumed"}})
                    continue
                result = prepared["result"]
            elif prepared["before"] != checkout_fingerprint():
                result = {"outcome": "conflict"}
            elif method == "vcs/reconcileCheckoutOperation":
                result = {"outcome": "notApplied"}
            else:
                if operation == "create":
                    checkout = checkout_descriptor("fixture-created:" + request["name"], request["name"], "mounted", True)
                    checkout["branch"] = request["branch"]
                    if args.scenario == "checkout-preparation-failed":
                        checkout.update(preparation="failed", preparationError="Fixture project preparation failed")
                    checkout_entries[checkout["id"]] = checkout
                    send_progress(token, "created", "Checkout registered")
                else:
                    checkout = dict(checkout_entries[request["checkoutId"]])
                    if operation == "remove":
                        del checkout_entries[checkout["id"]]
                        checkout = None
                    else:
                        checkout["state"] = "unmounted" if operation == "suspend" else "mounted"
                        checkout["operations"] = ["open", "prepare"] if operation == "suspend" else ["open", "prepare", "suspend", "remove"]
                        checkout["preparation"] = "ready"
                        checkout.pop("preparationError", None)
                        checkout_entries[checkout["id"]] = checkout
                result = {"outcome": "applied"}
                if checkout is not None:
                    result["checkout"] = checkout
                prepared["result"] = result
                checkout_count += 1
                persist_checkouts()
                record("checkout-operation-applied", operation=operation, count=checkout_count,
                       checkout_id=checkout["id"] if checkout else request.get("checkoutId"))
                if operation in ("create", "open", "prepare"):
                    send_progress(token, "preparing", "Checking project availability")
                send_progress("spoofed-token", "spoofed", "This update must not reach the store")
                if args.scenario == "checkout-pending":
                    record("checkout-apply-held", id=identifier)
                    threading.Thread(target=release_reply, args=(identifier, result, "checkout-apply-released"), daemon=True).start()
                    continue
                if args.scenario == "checkout-unknown":
                    reply(identifier, error={"code": -32018, "message": "Checkout acknowledgement lost", "data": {"kind": "outcomeUnknown"}})
                    continue
            if prepared["result"] is None:
                prepared["result"] = result
                persist_checkouts()
            if args.scenario == "checkout-invalid-result" and result.get("checkout"):
                result = json.loads(json.dumps(result))
                result["checkout"]["rootUri"] = (checkout_base / "wrong-checkout").as_uri()
                result["checkout"]["scopeUri"] = (checkout_base / "wrong-checkout" / "project").as_uri()
            reply(identifier, result)
        elif method == "shutdown":
            record("shutdown-received")
            reply(identifier)
        else:
            reply(identifier, error={"code": -32601, "message": "Unknown fixture method"})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
