# Tool Layer Design (typed tools, not just commands)

Design notes for turning Mothership's tool execution from a single `run_command`
shell primitive into a **typed, first-class tool layer** — with `read_file`,
`write_file`, `edit_file`, `apply_patch` as native Core tools alongside `run_command`.

This **extends** [`EXECUTION_MODEL.md`](EXECUTION_MODEL.md) and [`PROCESS_SANDBOX.md`](PROCESS_SANDBOX.md):
the supervisor, output policy, approval gate, leases and `ProcessSandbox` described
there stay; this doc generalizes *what* runs through them.

> **Status: IMPLEMENTED + review-hardened on `feat/typed-tool-layer`.** All four file
> tools (`read_file`/`write_file`/`edit_file`/`apply_patch`) are wired end-to-end
> alongside an unchanged `run_command`, and survived 4 review rounds. `mothership-core`:
> 207 tests green; whole workspace (incl. the Tauri app) builds clean. Pure cores
> (`tools/file_edit.rs`, `tools/patch.rs`) were aligned to the canonical Codex/Claude
> implementations before wiring. **The "As shipped — final invariants" section below is
> the source of truth; the "starting line" and "(todo)" notes are historical design
> context.**

## The spine (two ideas)

**1. A tool is not a process.** `run_command` is *one* tool, not the definition of a
tool. `read_file`/`write_file`/`edit_file`/`apply_patch` are pure filesystem
operations — forcing them through `ToolCommand{program,args,env}` (shelling out to
`cat`/patch binaries) is wrong. Lift the abstraction: a `Tool` trait with a typed
invocation; the supervisor becomes a handler-agnostic pipeline.

**2. Tool declares intent; policy decides.** A tool describes *what it wants to do*
(capability + which paths it touches + risk). A separate policy engine decides
*whether it's allowed right now* (project, paths, protected globs, user settings,
remote mode) → `allow | ask | deny`. Not "all logic in the tool", not "one monolithic
policy" — the split is the point.

## As-built today (the starting line, HEAD 3edb9a3)

```text
adapter (adapters/*/src/chat.rs: converts Core ToolDescriptors -> provider JSON)
  -> core run.rs (chat loop, LlmToolCallHandler; tools: Vec<ToolDescriptor> in request)
  -> sidecar SidecarLlmToolHandler  (match name { "run_command" => .., _ => unsupported })
  -> core ToolSupervisor.run_command (repeat-guard -> permission -> approval
        -> leases -> ProcessSandbox.spawn -> bounded output / spill -> result)
```

Already done by recent commits (do NOT redo):
- Core-owned schema catalog: `tools/catalog.rs` `default_tool_catalog() -> Vec<ToolDescriptor>`.
- `LlmChatCompletionRequest` already carries `tools: Vec<ToolDescriptor>`; adapters
  (`adapters/openrouter/src/chat.rs`, `adapters/codex/src/chat.rs`) only translate them.
- Batch concurrency policy: `tools/scheduler.rs` (`tool_batch_plan`); non-`run_command`
  defaults to `Exclusive` — so new file tools are safely serialized until refined.

Still command-centric / to change:
- `tools/types.rs` `ToolExecutionRequest` wraps a single `ToolCommand`.
- Permission is a **command-string** classifier (`ConservativeCommandPermissionPolicy`).
- Sidecar handler hardcodes the `run_command` name + `RunCommandArguments` parsing.
- `scheduler.rs` / `repeat_guard.rs` hardcode `RUN_COMMAND_TOOL_NAME`.
- Storage is `chat_tool_events.command_json` + `result_json` (`database.rs`) — not typed.

Reusable and good already: bounded output + spill-to-file (`ToolOutputPolicy`/
`ToolOutputStore`), approval-as-event (`PendingToolApprovalGate` + `PermissionRequested`),
repeat-guard, resource leases, cancellation, event sink.

## Target shape

As-shipped layout (the planned `invocation.rs`/`dispatcher.rs`/`effects.rs` were NOT
created — dispatch landed as an additive `FileToolRunner` in the sidecar handler, and
outcomes are a `FileToolOutcome` struct rather than a separate `effects.rs`):

```text
crates/mothership-core/src/tools/
  catalog.rs       // ToolDescriptor schemas for all 5 tools (LANDED)
  file_edit.rs     // pure apply_edit matcher (LANDED)
  patch.rs         // pure parse_v4a + plan_patch + apply rollback helpers (LANDED)
  filesystem.rs    // Workspace (resolve+containment+sensitive+Windows compare)
                   //   + FileSystem port + StdFileSystem (LANDED)
  file_tools.rs    // read_file/write_file/edit_file/apply_patch handlers,
                   //   FileToolOutcome, capability classify, preview_diff (LANDED)
  permissions.rs   // PendingToolApprovalGate + request_decision(tool_call_id) (LANDED)
src-sidecar/src/tool_runtime.rs
  FileToolRunner   // dispatch 4 tools: classify -> approval(event) -> execute
                   //   -> bounded events; run_command path unchanged (LANDED)
```

**Generic invocation.** The existing `LlmToolCallRequest{ run_id, tool_call_id, name,
arguments: Value }` already *is* a generic envelope. The remaining work is a registry
+ pipeline that dispatches by `name` to a typed handler instead of the hardcoded
`run_command` match; `run_command` becomes one handler among many. The supervisor keeps
every cross-cut (repeat-guard, permission, approval, leases, output policy, events,
cancellation); only the "execute" step becomes pluggable.

**Where IO lives.** Edit/patch *algorithms* and the path *policy* live in Core
(`file_edit.rs` / `patch.rs` already pure; a `filesystem.rs` resolver next). Raw byte IO
goes through an injected **`FileSystem` port** (default = `std::fs` at the sidecar
composition root), mirroring how `ProcessSandbox` injects process spawning. Lets Core
unit-test against an in-memory FS.

## Tool-use flow (target)

```text
1. ChatRunService asks Core ToolCatalog for the project/model's tools.   (exists)
2. Core sends stable, sorted schemas in LlmChatCompletionRequest.        (exists)
3. Adapter converts schemas to provider format.                         (exists)
4. Model emits a tool call.
5. Adapter -> sidecar -> Core dispatcher (registry lookup by tool_name). (todo)
6. Dispatcher: validate input -> resolve project path -> classify (tool) ->
   decide (policy) -> pending approval if needed (event; UI responds).
7. Tool executes; writes durable typed events/effects/artifacts; returns a
   bounded result to the model.
8. UI listens to events and sends approve/deny/cancel. Approval card for a
   mutating tool MUST show the diff before the Approve button.
```

## The four file tools

### read_file
- Input: `{ path, startLine?, limit?, maxBytes? }`.
- Resolve via project path resolver (canonicalize + symlink + containment). Read via `FileSystem` port.
- **Binary guard**: detect binary, do NOT stream bytes into context — return
  `{ error: "file appears to be binary", size, hint }`.
- Line-numbered content **for display/reasoning only** (never the edit key). Large files
  reuse the spill: preview + tail + `contentRef` + `truncated` + line count.
- Result: `{ path, sha256, totalLines, startLine, endLine, content, truncated, contentRef }`.
- Capability: read. Auto-allow normal project files; ask/deny sensitive + outside project.

### write_file
- Input: `{ path, content, create?, overwrite?, expectedSha256? }`.
- Carries the base mutation machinery (built here first, reused by edit/apply_patch):
  read old -> diff -> check sha -> approval(diff) -> **atomic write** -> effect/audit -> result.
- **Atomic write**: temp file beside target -> fsync where possible -> `std::fs::rename`
  (which replaces an existing dest on all platforms incl. Windows — no `ReplaceFile`/
  `MoveFileEx` needed). Old file is stream-hashed (never slurped); diff summarized when large.
- Overwrite of an existing file: `expectedSha256` effectively required; if missing and the
  file exists, treat as full-overwrite → ask with explicit warning. Mismatch → conflict, do not write.
- Result: `{ path, created|modified, bytes, +N/-N, sha256 }`.

### edit_file  (pure core LANDED: `tools/file_edit.rs::apply_edit`)
- Input: `{ path, oldText, newText, replaceAll?, expectedSha256? }`. Content-addressed,
  **not** line-numbers (line numbers go stale).
- v1: exact `oldText`, **exactly one match** (0 → stage 2; >1 → `Ambiguous` with snippets).
- v1.5: narrow fallback on whitespace/indent normalization, still exactly one match.
- **Not** aggressive fuzzy. Model either hits exactly or gets a clear error and re-reads.
  `expectedSha256` recommended. Result: diff summary + final `sha256`. Repeat-guard
  suppresses the retry loop on deterministic "not found / ambiguous".

### apply_patch  (pure core LANDED: `tools/patch.rs::parse_v4a` + `plan_patch`)
- Input: `{ patch }` — strict V4A grammar: add / update / delete / move, multi-file.
- **`plan_patch` is the dry-run + content check**: all hunks must match current content;
  any failure → abort whole patch, write nothing (all-or-none).
- Apply all-or-none **on disk**: snapshot prior bytes/existence of every touched path + the
  dirs it will create; on any mid-apply IO error, restore files and remove created dirs.
- One batched approval showing the file list + unified diff. Delete/move → higher-risk.
- Dry-run *is* the content check — do not bolt a redundant `expectedSha256` onto apply_patch.

### Role split
`apply_patch` = primary for multi-file / complex changes. `edit_file` = pointwise
replacement. `write_file` = create / full overwrite only. `read_file` = context.
`run_command` = tests / build / git / diagnostics.

## Typed storage (extends `command_json`)

```text
tool_calls:     id, run_id, chat_id, message_id, tool_name, input_json,
                status, permission_state, created_at, updated_at
tool_events:    id, tool_call_id, kind, message, payload_json, occurred_at
tool_artifacts: id, tool_call_id, kind, path, byte_size, sha256, storage_ref
```

Keep `chat_message_parts` (already orders text→tool→text). `tool_artifacts` ties to the
existing spill `log_ref`, to read_file `contentRef`, and to apply_patch snapshots.

## UI (keep inline pattern, change rendering)

Semantic cards, not terminal cards: `read_file` (path, lines/bytes, collapsed preview);
`write_file` (created/modified, diff on expand); `edit_file` (+N/-N, changed hunk);
`apply_patch` (N files, +N/-N, per-file diff). Mutating-tool approval card shows the diff
before Approve. Approval is a Core event routed to wherever the human is (incl. remote mode).

## Build order

```text
0. [DONE] pure cores: file_edit.rs (apply_edit), patch.rs (parse_v4a/plan_patch) + tests.
0b.[DONE] aligned both cores to canonical Codex/Claude behaviour (seek_sequence fuzzy,
          EOF, @@-seek, quote/escape normalization) with fixtures from codex tests.
1. [DONE] filesystem.rs: Workspace (path resolve + containment + sensitive-path policy)
          + FileSystem port + StdFileSystem (atomic write = temp + fsync + rename).
2. [DONE] read_file  (binary guard, cat -n line numbers, offset/limit, large-output spill).
3. [DONE] write_file (create/overwrite, expectedSha256 conflict, BOM/EOL preserve, atomic).
4. [DONE] edit_file  (wires apply_edit; atomic write; NotFound/Ambiguous as ok:false).
5. [DONE] apply_patch (wires plan_patch; guard all paths; plan-level all-or-none; batched
          approval; per-file apply).
+  [DONE] capability policy (read=allow / mutate=ask / outside+sensitive=deny) + sidecar
          dispatch through the shared PendingToolApprovalGate; run_command 1:1.
6. [DONE] review hardening (4 rounds): non-UTF-8 refusal; apply_patch snapshot+rollback
          incl. created dirs; bounded event/approval-preview payloads + spill; read_file
          hard cap + honest paging; write_file stream-hash + bounded/summary diff (old AND
          new); edit_file >10 MiB guard; Windows path compare; Claude native Read/Write/
          Edit/MultiEdit/NotebookEdit routing.

=== file-tool stage CLOSED here ===

7. [DONE] typed search: native `list_files` + `search_text` (`ignore`+`regex`+`globset`,
          NO shell/`rg`); gitignore-default + `includeIgnored`, sensitive ALWAYS skipped,
          `dir`/`glob` contained, bounded results; Claude-native `Glob`/`Grep` disabled when
          both are bridged. read/Allow/ParallelSafe.
8. [later] scheduler: read_file -> ParallelSafe (small standalone follow-up, not bundled).
9. [later] typed DB storage (tool_calls/tool_events/tool_artifacts) + UI semantic cards.
```

Implementation note: rather than genericizing `ToolSupervisor` into one trait now, the
file tools run through an additive `FileToolRunner` in the sidecar handler that reuses the
existing approval gate + event sink; `run_command` keeps its supervisor path untouched.
This list extends the [`MIGRATION_PLAN.md`](MIGRATION_PLAN.md) checklist.

## As shipped — final invariants (source of truth)

These hold on `feat/typed-tool-layer` as of the 4th review round. They supersede any
"limitation" wording elsewhere in this doc.

- **UTF-8 safety (no data loss).** `edit_file` / `apply_patch` use *strict* `from_utf8`
  and REFUSE non-UTF-8 content (no lossy U+FFFD rewrite of e.g. cp1251 files). `write_file`
  is a full overwrite of model-supplied (valid) content. `read_file` may lossily *display*
  text but flags `lossy`/`linesAreFromCappedPrefix` and never writes.
- **Bounded memory, everywhere.** `read_file` hard-caps the raw read at 10 MiB
  (`MAX_READ_FILE_BYTES`); `maxBytes` only governs inline render (clamped down, schema
  `maximum`). `write_file` stream-hashes the old file (never slurps it), reads only a
  capped prefix for BOM/EOL+diff, and emits a *summary* diff when old+new > 1 MiB
  (`MAX_DIFF_INPUT_BYTES`) — covers both large-old and large-new. `edit_file` refuses files
  > 10 MiB (`status: too_large`, points to `apply_patch`/`run_command`), since it must load
  the whole file to apply a content-addressed edit.
- **read_file paging is honest.** `startLine`/`limit` page *within* the 10 MiB window only;
  the tool does NOT pretend to page past the cap — it points to `run_command` (`sed -n` /
  `Get-Content`) for ranges in larger files.
- **apply_patch is all-or-none on disk.** `plan_patch` is the dry-run content check; apply
  snapshots prior bytes/existence of every touched path AND the directories it will create,
  and on any mid-apply IO error restores files + removes newly-created dirs (deepest-first).
- **Bounded events/DB/approval.** Diff/result text persisted/streamed and the
  PermissionRequested *preview* diff are bounded to `MAX_TOOL_EVENT_BYTES` (64 KiB), spilling
  the full diff to a `logRef`. `expectedSha256` gives optimistic-concurrency conflict checks.
- **Claude adapter routing.** When the Mothership file tools are bridged (as
  `mcp__mothership__*`), the Claude-agent adapter disables Claude's native `Write`/`Edit`/
  `MultiEdit`/`NotebookEdit` (gated on a Mothership file tool present) AND native `Read`
  (gated on `read_file` present, so the model is never left without a read path) — so
  reads+writes flow through the typed pipeline. Native `Glob`/`Grep` are disabled when both
  typed search tools (`list_files`+`search_text`) are bridged. `Bash` is disabled when
  `run_command` is present.

## Deferred (NOT defects — scope, stage considered closed without them)

- **Typed search — DONE.** Native `list_files`/`search_text` (read-only, `ParallelSafe`) ship
  alongside the file tools; Claude-native `Glob`/`Grep` are disabled when both typed search
  tools are bridged. No shell/`rg` — `ignore`+`regex`+`globset` crates.
- **Typed storage** (`tool_calls`/`tool_events`/`tool_artifacts`) — events still via
  `chat_tool_events` (`command_json` NULL for file tools).
- **UI semantic cards** + diff-before-approve card — frontend; approval diff currently rides
  (bounded) in the event `message`.
- **`scheduler.rs`** classifies non-`run_command` tools `Exclusive`; `read_file` →
  `ParallelSafe` is a small standalone follow-up (do NOT bundle with typed search).
- **`ToolSupervisor` not genericized** — file tools use an additive `FileToolRunner`
  (deliberate, to avoid destabilizing `run_command`).
- Optional later policy: a sanity ceiling on `write_file.content` size.

## Decision log (so it isn't re-litigated)

| Considered | Decision | Why |
| --- | --- | --- |
| Do file ops via `run_command` (cat / patch binaries) | **Rejected** | Non-portable, unapprovable as typed effects, no diff/conflict semantics. File ops are first-class tools. |
| Extend `ToolExecutionRequest{command}` for new tools | **Rejected** | Dispatch by the existing `name`+`arguments` envelope to typed handlers; `run_command` is one tool. |
| `write_file` last (overwrite semantics are subtle) | **Rejected → second** | It's the cheapest carrier for the mutation/diff-approval/atomic-write machinery that edit/apply_patch reuse. Build it second, carefully. |
| Aggressive 9-stage fuzzy matcher for `edit_file` (opencode-style) | **Rejected → strict + narrow fallback** | Aggressive fuzzy can confidently patch the *wrong* region. Default exact+unique; v1.5 whitespace/indent-only fallback; uniqueness always enforced. (Implemented in `file_edit.rs`.) |
| line-number-based edits as the primary API | **Rejected** | Line numbers go stale after any shift. Content-addressed `oldText`; line-range only later, gated by `expectedSha256`. |
| `classify()` all-in-tool **or** all-in-one-policy | **Split** | Tool declares intent/capability/paths; policy engine decides allow/ask/deny with project/path/settings/remote context. |
| `expectedSha256` on every mutation incl. apply_patch | **Refined** | Required-ish for write_file overwrite; recommended for edit_file; **redundant for apply_patch** (`plan_patch` all-hunks-match is the content check). |
| Per-adapter hardcoded tool schemas | **Rejected (already fixed)** | Core `catalog.rs` owns schemas; adapters only convert format. |
| read_file streams any file | **Rejected** | Binary guard: detect + refuse with size + hint; large text spills (preview+tail+contentRef). |
| Atomic write needs `ReplaceFile`/`MoveFileEx` on Windows | **Corrected** | `std::fs::rename` already REPLACES an existing dest on all platforms incl. Windows (verified). `StdFileSystem::write_atomic` = temp-in-same-dir + fsync + `fs::rename`. |
| read_file streaming line-reader to page past the 10 MiB cap | **Rejected → honest cap** | Inline-reading >10 MiB through the model is an anti-pattern; `run_command` (sed/Get-Content) covers ranges in huge files. read_file caps + tells the truth instead of faking paging. |
| `edit_file` reads any-size file | **Rejected → >10 MiB guard** | edit_file must load the whole file (content-addressed edit); refuse >10 MiB and point at `apply_patch`/`run_command`. Mirrors the read_file ceiling. |
| `write_file` full diff regardless of size | **Rejected → bounded** | Summary diff when old+new > 1 MiB (`MAX_DIFF_INPUT_BYTES`) — covers large-NEW content too, not just large-old. No schema cap on `content` (legit large writes; the diff was the real cost). |
| apply_patch rollback covers files only | **Rejected → files + dirs** | Snapshot also records dirs the apply will create; rollback removes them deepest-first (empty-only). True "disk as before". |
| Disable Claude-native `Glob`/`Grep` now | **Deferred** | No typed search replacement yet; disabling would degrade search to `run_command`. Disable only after `list_files`/`search_text` land. Native `Read`/`Write`/`Edit`/`MultiEdit`/`NotebookEdit` ARE disabled when the Mothership file tools are bridged. |

## Open questions

- Should `edit_file`/`apply_patch` run a formatter / surface LSP diagnostics post-write
  (opencode-style ambient feedback)? Deferred — not Stage 1.
- Refine `scheduler.rs` so read_file is `ParallelSafe` while mutations stay `Exclusive`
  (currently all non-`run_command` default to `Exclusive` — safe but conservative).
- Deferred tools / `tool_search` (BM25) only matter once MCP/dynamic tool catalogs are
  large; core file tools never defer. Out of scope here.
