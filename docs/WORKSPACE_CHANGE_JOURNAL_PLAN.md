# Workspace Change Journal Plan

## Зачем

Mothership нужен пользовательский слой изменений проекта: блоки вида “Изменено N файлов”, “Проверить”, “Отменить”, per-file diff и безопасный rollback. Это не UI-улучшение для тул-вызовов, а продуктовая возможность Core.

Цель: пользователь должен видеть, что агент поменял в workspace, быстро ревьюить изменения и откатывать их без ручного поиска по файлам.

## Главный принцип

```text
Agent Core owns workspace changes.
UI renders change summaries and sends commands.
Model context stays unchanged.
```

Workspace Change Journal не должен строиться из frontend payload'ов, markdown-ответов модели или текущих `tool_artifacts`. Эти данные полезны для отображения, но не являются источником истины для отката.

## Reference Findings

Лучшие источники из `projects-to-research`:

- `opencode`: side-git snapshots до/после шага, persisted patch parts, revert/unrevert.
- `hermes-agent`: shadow git checkpoint store вне пользовательского `.git`, diff/restore, pruning, RPC/CLI rollback.
- `deepseek-tui`: snapshots `pre-turn`, `post-turn`, `tool:<call_id>`, `/restore`, `/undo`, `revert_turn`.
- `claude`: full-file backup history per user message, simple rewind, no micro-git.
- `codex`: exact `apply_patch` deltas and turn diff tracker, strong patch/diff contract.

Вывод: для Mothership лучший вариант — Core-owned shadow snapshot engine + SQLite change journal + conflict-aware restore. Claude-style full-file backups можно использовать как упрощение или fallback, но не как конечную архитектуру.

## Product Shape

В чате должен появляться единый summary-блок изменений:

```text
Изменено 13 файлов
+448 -263

crates/mothership-core/src/lib.rs        +15 -1
src/features/dashboard/Dashboard.tsx     +34 -12
src/App.css                              +88 -20

Показать еще 10 файлов

Отменить   Проверить
```

Свернуто:

- человеческий summary;
- число файлов;
- общий diff stat;
- первые файлы;
- статус rollback/review.

Раскрыто:

- per-file diff;
- lazy diff loading;
- file status `A/M/D/R`;
- конфликтные файлы;
- ссылки “Открыть”.

## Architecture

Новый слой должен жить в Core:

```text
src/
  UI only: render summaries, call commands, subscribe events

src-tauri/
  thin adapter: forward commands/queries/events to sidecar

src-sidecar/
  Core host: wire protocol and service wiring

crates/mothership-core/
  changes/
    commands
    queries
    events
    snapshot engine
    repositories
    domain models
```

Suggested Core module name:

```text
changes
```

Alternative names:

- `workspace_changes`
- `file_history`
- `checkpoints`

Preferred: `changes`, because the feature is broader than files only. It belongs next to `runs`, `tools`, `projects`, and `storage`.

## Domain Model

Core concepts:

```text
WorkspaceSnapshot
ChangeSet
ChangeFile
ChangeRevert
ChangeConflict
```

`WorkspaceSnapshot`:

- `id`
- `project_id`
- `worktree_path`
- `snapshot_ref`
- `kind`: `pre_run`, `pre_turn`, `pre_tool`, `post_tool`, `post_turn`, `manual`
- `run_id`
- `message_id`
- `tool_call_id`
- `created_at`

`ChangeSet`:

- `id`
- `project_id`
- `run_id`
- `message_id`
- `tool_call_id`
- `before_snapshot_id`
- `after_snapshot_id`
- `status`: `active`, `reverted`, `restored`, `conflicted`, `stale`
- `file_count`
- `additions`
- `deletions`
- `created_at`
- `updated_at`

`ChangeFile`:

- `id`
- `change_set_id`
- `path`
- `old_path`
- `op`: `A`, `M`, `D`, `R`
- `additions`
- `deletions`
- `before_hash`
- `after_hash`
- `diff_ref`
- `is_binary`
- `is_large`

`ChangeRevert`:

- `id`
- `change_set_id`
- `status`: `started`, `completed`, `conflicted`, `failed`
- `created_at`
- `completed_at`
- `error`

`ChangeConflict`:

- `id`
- `revert_id`
- `path`
- `reason`: `current_hash_mismatch`, `missing_file`, `unexpected_file`, `permission_denied`, `outside_workspace`
- `expected_hash`
- `actual_hash`
- `details`

## Storage

SQLite owns metadata and relationships.

Large payloads live outside SQLite:

```text
app-data/
  snapshots/
  change-diffs/
  change-blobs/
```

Shadow snapshot storage should not touch the user's `.git`.

Preferred backend:

```text
private git object store / shadow git repo
```

Requirements:

- separate storage per project/worktree;
- no writes to user `.git`;
- respect workspace containment;
- respect ignore policy where appropriate;
- cap very large files;
- binary files are tracked by hash/status but diff rendering is disabled;
- retention/pruning is explicit and observable.

## Snapshot Engine

Core should expose a `WorkspaceSnapshotStore` port:

```text
trait WorkspaceSnapshotStore {
    fn capture(&self, request: CaptureSnapshotRequest) -> Result<WorkspaceSnapshotRef>;
    fn diff(&self, from: SnapshotRef, to: SnapshotRef) -> Result<WorkspaceDiff>;
    fn restore_files(&self, request: RestoreFilesRequest) -> Result<RestoreResult>;
    fn read_file_at(&self, snapshot: SnapshotRef, path: WorkspacePath) -> Result<Option<FileBytes>>;
    fn prune(&self, policy: RetentionPolicy) -> Result<PruneReport>;
}
```

Implementation can use git internally, but the domain API must not leak git commands.

## Capture Semantics

Snapshots should be created at these boundaries:

- before run starts;
- before each mutating tool;
- after each mutating tool;
- after turn finishes;
- before manual revert/restore.

Mutating tools include:

- `write_file`;
- `edit_file`;
- `apply_patch`;
- future `multi_edit`;
- destructive or unknown `run_command`;
- any tool with file-system write capability.

Initial policy:

- tool-level snapshots for direct file tools;
- turn-level snapshots for user-facing summary;
- optional command-level snapshots for `run_command`, depending on capability/risk.

## ChangeSet Creation

After a mutating operation finishes:

1. Capture `after_snapshot`.
2. Diff `before_snapshot -> after_snapshot`.
3. If no files changed, do not create a visible `ChangeSet`.
4. Persist `ChangeSet`.
5. Persist `ChangeFile` rows.
6. Persist diff refs for lazy loading.
7. Emit `change_set.created`.

If a tool fails after partial writes, still capture `after_snapshot` and create a `ChangeSet` with `status = active` plus tool failure metadata. The user still needs to see and possibly revert partial changes.

## Revert Semantics

Revert is not “apply reverse diff blindly”.

Before writing anything:

1. Load `ChangeSet`.
2. For every affected file, compare current state with expected `after_hash`.
3. If all files match, restore affected files from `before_snapshot`.
4. If any file does not match, do not overwrite it silently.
5. Persist conflicts and emit events.

Rules:

- `A`: delete file only if current hash equals `after_hash`.
- `D`: restore file from `before_snapshot` only if current file is still absent or matches expected state.
- `M`: write before content only if current hash equals `after_hash`.
- `R`: restore old path/new path with the same hash checks.

This protects user edits made after the agent's change.

## Restore / Unrevert

If a user reverts and then wants to restore the agent's changes:

1. Capture snapshot before restore.
2. Compare current state with expected reverted state.
3. Restore affected files from `after_snapshot`.
4. Persist `ChangeRevert` / restore status.
5. Emit `change_set.restored`.

This mirrors opencode's revert/unrevert model but keeps the API product-level, not git-level.

## Commands

Remote-first Core commands:

```text
create_workspace_snapshot
revert_change_set
restore_change_set
mark_change_set_reviewed
prune_change_snapshots
```

Most snapshots should be created internally by the run/tool pipeline. Public commands are mainly for manual actions and debug/admin flows.

## Queries

```text
list_change_sets(project_id, limit, cursor)
get_change_set(change_set_id)
get_message_change_summary(message_id)
get_run_change_summary(run_id)
get_change_file_diff(change_file_id, offset, limit)
get_snapshot_file(snapshot_id, path, offset, limit)
list_change_conflicts(change_set_id)
```

All large diff/file queries must be paginated or range-based.

## Events

```text
change_set.created
change_set.updated
change_set.reviewed
change_set.revert_started
change_set.reverted
change_set.restore_started
change_set.restored
change_set.conflicted
snapshot.created
snapshot.pruned
```

Desktop and future phone clients subscribe to the same event stream.

## UI Contract

The Dashboard should not reconstruct workspace state from tool output.

It should receive:

```ts
type ChangeSetSummary = {
  id: string;
  status: "active" | "reverted" | "restored" | "conflicted" | "stale";
  fileCount: number;
  additions: number;
  deletions: number;
  files: Array<{
    id: string;
    path: string;
    oldPath?: string;
    op: "A" | "M" | "D" | "R";
    additions: number;
    deletions: number;
    isBinary: boolean;
    isLarge: boolean;
  }>;
};
```

UI actions:

- `Проверить`: open review panel for the change set.
- `Отменить`: call `revert_change_set`.
- `Восстановить`: call `restore_change_set` for reverted sets.
- `Показать еще`: query more files.
- Expand file: lazy-load diff range.

## Relationship To Tool Artifacts

Current tool artifacts can remain as execution details:

- command output;
- read output;
- tool-local preview;
- logs.

Workspace Change Journal owns:

- what changed;
- how to review it;
- how to revert it;
- whether revert is safe.

Do not make `tool_artifacts` the rollback source of truth.

## Relationship To Model Context

No model-channel change is required.

The model still receives normal bounded tool results. Change journal events and snapshots are product/runtime metadata. They should not be injected into prompts unless a future explicit feature asks for it.

## Capability And Permission Policy

Snapshot/revert must follow the same workspace containment policy as file tools.

Rules:

- UI never passes raw filesystem paths for restore.
- Core resolves project/workspace paths.
- Restore refuses paths outside workspace.
- Destructive restore should be approval-gated when initiated by the agent.
- User-initiated restore can still show confirmation when conflicts or many files are involved.

## Retention

Snapshots can grow. The system needs explicit retention:

- keep recent active project snapshots;
- keep snapshots referenced by visible chat history;
- prune unreachable snapshots;
- cap total snapshot storage size;
- expose storage usage in settings later.

Initial defaults:

- never prune snapshots referenced by non-archived chats;
- allow manual cleanup later;
- log pruning events.

## First Implementation Slice

Goal: prove the vertical architecture with direct file tools.

Scope:

- Core `changes` module;
- SQLite metadata;
- snapshot store skeleton;
- capture before/after for `write_file`, `edit_file`, `apply_patch`;
- create `ChangeSet`;
- query message/run change summary;
- UI summary block;
- lazy per-file diff;
- revert with hash conflict checks.

Out of scope for slice 1:

- full `run_command` filesystem diff;
- binary file restore UI;
- global storage cleanup UI;
- restore/unrevert polish;
- mobile UI.

## Second Slice

Goal: make rollback robust for broader agent behavior.

Scope:

- turn-level snapshots;
- `run_command` snapshots for commands with write capability;
- restore/unrevert;
- conflict UI;
- pruning policy;
- review panel;
- event stream integration for remote clients.

## Third Slice

Goal: production hardening.

Scope:

- large repo performance;
- ignore policy;
- binary/large file policy;
- snapshot storage migration;
- settings for retention;
- diagnostics and repair;
- crash recovery tests;
- Windows path edge cases;
- concurrent run isolation.

## Testing Strategy

Core tests:

- create snapshot before/after write;
- diff detects `A/M/D/R`;
- revert added file;
- revert modified file;
- revert deleted file;
- conflict on current hash mismatch;
- no overwrite outside workspace;
- large file is tracked without rendering full diff;
- binary file is tracked without text diff;
- failed tool with partial write still creates change set.

Integration tests:

- `write_file` creates visible change set;
- `edit_file` creates correct diff stat;
- `apply_patch` creates per-file stats;
- revert command updates workspace and emits events;
- conflict event reaches UI query model.

UI tests:

- summary renders first files only;
- “show more” loads more files;
- diff loads lazily;
- reverted/conflicted states render clearly;
- open/review/revert buttons do not block chat rendering.

## DoD

- Change summary is rendered from `ChangeSet`, not tool payload.
- Revert is Core-owned and conflict-aware.
- UI never reads or writes workspace files directly.
- Snapshot storage does not touch user `.git`.
- Large diffs are lazy-loaded.
- Model context remains unchanged.
- Desktop API is remote-first and suitable for future phone client.
- `npm run build` passes.
- `cargo check --workspace` passes.
- Core tests cover snapshot/diff/revert/conflict.

## Agent Handoff

This document is intended to be enough context for another coding agent to start implementation without re-deriving the product decision.

Before coding, the agent must read:

- `docs/PROJECT_DESCRIPTION.md`
- `docs/ARCHITECTURE.md`
- this document
- current tool implementation under `crates/mothership-core/src/tools/`
- current sidecar IPC types under `crates/mothership-core/src/ipc.rs`
- current frontend dashboard tool rendering under `src/features/dashboard/`

Repository constraints:

- Do not use `rg`; use PowerShell `Get-ChildItem`, `Select-String`, or direct file reads.
- Do not modify `projects-to-research/`.
- Keep Tauri command handlers thin.
- Keep product behavior in Core.
- Keep UI as a client of commands, queries, and events.
- Do not change the model tool-output channel unless explicitly required by a later design.

Current repo context:

- Core lives in `crates/mothership-core/`.
- Sidecar host lives in `src-sidecar/`.
- Tauri desktop adapter lives in `src-tauri/`.
- SolidJS frontend lives in `src/`.
- Existing tool output/artifact logic is useful for display, but is not a safe rollback source of truth.
- Existing file tools already know which operations are mutating; use that pipeline as the first integration point.

Recommended first PR:

1. Add a Core `changes` module with domain types and repository interfaces.
2. Add SQLite metadata tables for snapshots, change sets, change files, reverts, and conflicts.
3. Add an initial snapshot store implementation behind a port.
4. Wire capture before/after around `write_file`, `edit_file`, and `apply_patch`.
5. Persist `ChangeSet` and `ChangeFile` rows from the resulting diff.
6. Add queries for `get_message_change_summary` and lazy per-file diff read.
7. Add a Core command `revert_change_set` with hash/existence conflict checks.
8. Add sidecar IPC wrappers for the new queries/commands.
9. Render the Dashboard summary block from `ChangeSetSummary`.
10. Add focused Core tests before broad UI polish.

Do not start with:

- a Dashboard-only implementation;
- reverse-applying frontend diffs;
- storing rollback state only in tool artifacts;
- relying on the user's `.git`;
- loading full diffs/files into chat rows;
- implementing full `run_command` workspace tracking before direct file tools work end to end.

Suggested first-slice acceptance test:

```text
Given an existing project file
When the agent edits it through edit_file
Then Core captures before/after snapshots
And persists one active ChangeSet
And Dashboard shows “Изменено 1 файл” with +N -M
And expanding the file lazy-loads the diff
And pressing “Отменить” restores the previous file content
And the ChangeSet status becomes reverted
And no model context/output contract changes
```

Key design risk:

Snapshot storage is infrastructure, but rollback policy is domain behavior. Keep the storage implementation swappable, but keep conflict rules, status transitions, and user-visible events in Core.
