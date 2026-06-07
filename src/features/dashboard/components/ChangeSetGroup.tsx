import { For, Match, Show, Switch, createSignal } from "solid-js";

import {
  getChangeFileDiff,
  listChangeSetFiles,
  type ChangeFileDiff,
  type ChangeFileSummary,
  type ChangeSetSummary,
  type ChangeSetStatus,
  type RevertOutcome,
} from "../../../shared/api/mothership";
import { FileActions, onFileContextMenu } from "../../../shared/ui/FileActions";
import { CollapsibleDiff } from "./CollapsibleDiff";

// How many files the grouped block shows before "показать ещё". Set above a
// typical edit burst so a handful of single-file changes all show at once.
const GROUP_FILE_LIMIT = 8;
// Files beyond each set's inline preview are paged in this many at a time, so even
// a change set touching hundreds of files never loads them all in one request.
const FILE_PAGE_SIZE = 50;

/** Russian noun agreement for "файл" (1 файл / 2 файла / 5 файлов). */
function pluralFiles(count: number): string {
  const mod10 = count % 10;
  const mod100 = count % 100;
  if (mod10 === 1 && mod100 !== 11) return "файл";
  if (mod10 >= 2 && mod10 <= 4 && (mod100 < 10 || mod100 >= 20)) return "файла";
  return "файлов";
}

function statusLabel(status: ChangeSetStatus): string | null {
  switch (status) {
    case "reverted":
      return "Отменено";
    case "restored":
      return "Восстановлено";
    case "conflicted":
      return "Конфликт";
    case "stale":
      return "Устарело";
    default:
      return null;
  }
}

/**
 * One mutated file: a click toggles a lazily-loaded unified diff. Binary/large
 * files report why no diff is shown rather than fetching one.
 */
function ChangeFileRow(props: { file: ChangeFileSummary; projectId?: string }) {
  const [open, setOpen] = createSignal(false);
  // The whole-file diff (real line numbers, every unchanged line kept). Loaded
  // lazily on open; CollapsibleDiff shows only the changes ± context and unfolds
  // the collapsed gaps on click — no extra round-trip per expand.
  const [diff, setDiff] = createSignal<ChangeFileDiff | null>(null);
  const [state, setState] = createSignal<"idle" | "loading" | "error" | "ready">(
    "idle",
  );
  const file = () => props.file;
  const previewable = () => !file().isBinary && !file().isLarge;

  async function toggle() {
    const next = !open();
    setOpen(next);
    if (next && state() === "idle" && previewable()) {
      setState("loading");
      try {
        setDiff(await getChangeFileDiff(file().id, 0, 0, true));
        setState("ready");
      } catch {
        setState("error");
      }
    }
  }

  return (
    <li
      class="change-set__file"
      onContextMenu={(event) =>
        onFileContextMenu(event, {
          projectId: props.projectId,
          path: file().path,
        })
      }
    >
      <div class="change-set__file-head">
        <button
          class="change-set__file-row"
          type="button"
          onClick={() => void toggle()}
        >
          <span
            classList={{
              "change-set__op": true,
              [`change-set__op--${file().op}`]: true,
            }}
          >
            {file().op}
          </span>
          <span class="change-set__path">{file().path}</span>
        </button>
        <FileActions
          class="change-set__file-actions"
          projectId={props.projectId}
          path={file().path}
        />
        <span class="change-set__file-stat">
          <span class="change-set__add">+{file().additions}</span>{" "}
          <span class="change-set__del">−{file().deletions}</span>
        </span>
      </div>
      <Show when={open()}>
        <div class="change-set__diff">
          <Show
            when={previewable()}
            fallback={
              <p class="change-set__note">
                {file().isBinary
                  ? "Бинарный файл — diff не показывается."
                  : "Файл слишком большой для предпросмотра."}
              </p>
            }
          >
            <Switch
              fallback={<p class="change-set__note">Загрузка diff…</p>}
            >
              <Match when={state() === "error"}>
                <p class="change-set__note">Не удалось загрузить diff.</p>
              </Match>
              <Match when={state() === "ready" && diff()?.unavailable}>
                <p class="change-set__note">Diff недоступен.</p>
              </Match>
              <Match when={state() === "ready" && diff()}>
                <CollapsibleDiff
                  diff={(diff() as ChangeFileDiff).lines.join("\n")}
                />
              </Match>
            </Switch>
          </Show>
        </div>
      </Show>
    </li>
  );
}

/**
 * All workspace change sets produced by one assistant message, folded into a
 * single block: combined file count + diff stat in the header, one flat list of
 * every touched file (each row expands to its diff), and a single revert action
 * for the whole batch. Avoids a tall stack of one-file cards when the agent
 * writes several files in a turn. Reads Core-owned [`ChangeSetSummary`]s —
 * never reconstructed from tool output.
 */
export function ChangeSetGroup(props: {
  changeSets: ChangeSetSummary[];
  projectId?: string;
  onRevert: (changeSetId: string) => Promise<RevertOutcome>;
}) {
  const [expanded, setExpanded] = createSignal(false);
  const [reverting, setReverting] = createSignal(false);
  const [conflicts, setConflicts] = createSignal<RevertOutcome["conflicts"]>([]);
  const [error, setError] = createSignal<string | null>(null);
  // Files paged in beyond each set's inline preview, keyed by change-set id. The
  // summaries only carry the first few files so opening a chat stays bounded.
  const [extraFilesBySet, setExtraFilesBySet] = createSignal<
    Record<string, ChangeFileSummary[]>
  >({});
  const [loadingMore, setLoadingMore] = createSignal(false);

  // The caller hands sets in chronological order — preserve it.
  const sets = () => props.changeSets;
  const totalFiles = () => sets().reduce((sum, set) => sum + set.fileCount, 0);
  const additions = () => sets().reduce((sum, set) => sum + set.additions, 0);
  const deletions = () => sets().reduce((sum, set) => sum + set.deletions, 0);
  const activeSets = () => sets().filter((set) => set.status === "active");
  const hasActive = () => activeSets().length > 0;
  const toolFailed = () => sets().some((set) => set.toolFailed);

  // Distinct non-default status labels present across the batch (e.g. Отменено).
  const statusBadges = () => {
    const labels = new Set<string>();
    for (const set of sets()) {
      const label = statusLabel(set.status);
      if (label) labels.add(label);
    }
    return [...labels];
  };

  // The whole block greys out only when every set shares one terminal status;
  // a mixed batch keeps the active styling.
  const aggregateStatus = (): ChangeSetStatus => {
    const distinct = new Set(sets().map((set) => set.status));
    return distinct.size === 1 ? [...distinct][0] : "active";
  };

  // Every file loaded so far across all sets (inline preview + any paged-in).
  const loadedFiles = () =>
    sets().flatMap((set) => [...set.files, ...(extraFilesBySet()[set.id] ?? [])]);
  const visibleFiles = () =>
    expanded() ? loadedFiles() : loadedFiles().slice(0, GROUP_FILE_LIMIT);
  const hiddenCollapsed = () =>
    Math.max(0, totalFiles() - Math.min(GROUP_FILE_LIMIT, loadedFiles().length));
  const remaining = () => Math.max(0, totalFiles() - loadedFiles().length);

  // Page the next chunk of files for every set that still has some unloaded.
  async function loadMoreFiles() {
    if (loadingMore() || remaining() === 0) return;
    setLoadingMore(true);
    try {
      const pages = await Promise.all(
        sets().map(async (set) => {
          const loadedForSet =
            set.files.length + (extraFilesBySet()[set.id]?.length ?? 0);
          if (loadedForSet >= set.fileCount) return null;
          const more = await listChangeSetFiles(
            set.id,
            loadedForSet,
            FILE_PAGE_SIZE,
          );
          return [set.id, more] as const;
        }),
      );
      setExtraFilesBySet((current) => {
        const next = { ...current };
        for (const page of pages) {
          if (!page) continue;
          const [id, more] = page;
          next[id] = [...(next[id] ?? []), ...more];
        }
        return next;
      });
    } catch {
      // Best effort — the inline preview stays visible if paging fails.
    } finally {
      setLoadingMore(false);
    }
  }

  function expand() {
    setExpanded(true);
    void loadMoreFiles();
  }

  // Revert the batch: undo each still-active set in turn, gathering any conflicts
  // so the user sees exactly which files blocked the rollback.
  async function revertAll() {
    if (reverting() || !hasActive()) return;
    setReverting(true);
    setConflicts([]);
    setError(null);
    const collected: RevertOutcome["conflicts"] = [];
    try {
      for (const set of activeSets()) {
        const outcome = await props.onRevert(set.id);
        collected.push(...outcome.conflicts);
      }
      setConflicts(collected);
    } catch (caught) {
      // Core rolls the workspace back to its pre-revert state on failure, so the
      // change stays active; surface why it didn't apply.
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setReverting(false);
    }
  }

  return (
    <div
      classList={{
        "change-set": true,
        [`change-set--${aggregateStatus()}`]: true,
      }}
    >
      <div class="change-set__head">
        <span class="change-set__title">
          Изменено {totalFiles()} {pluralFiles(totalFiles())}
        </span>
        <span class="change-set__stat">
          <span class="change-set__add">+{additions()}</span>{" "}
          <span class="change-set__del">−{deletions()}</span>
        </span>
        <For each={statusBadges()}>
          {(label) => <span class="change-set__badge">{label}</span>}
        </For>
        <Show when={toolFailed()}>
          <span class="change-set__badge change-set__badge--warn">
            частичная запись
          </span>
        </Show>
      </div>

      <ul class="change-set__files">
        <For each={visibleFiles()}>
          {(file) => <ChangeFileRow file={file} projectId={props.projectId} />}
        </For>
      </ul>

      <Show when={!expanded() && hiddenCollapsed() > 0}>
        <button class="change-set__more" type="button" onClick={expand}>
          Показать ещё {hiddenCollapsed()} {pluralFiles(hiddenCollapsed())}
        </button>
      </Show>

      <Show when={expanded() && loadingMore()}>
        <p class="change-set__note">Загрузка файлов…</p>
      </Show>

      <Show when={expanded() && !loadingMore() && remaining() > 0}>
        <button
          class="change-set__more"
          type="button"
          onClick={() => void loadMoreFiles()}
        >
          Загрузить ещё {Math.min(FILE_PAGE_SIZE, remaining())}{" "}
          {pluralFiles(Math.min(FILE_PAGE_SIZE, remaining()))} (осталось{" "}
          {remaining()})
        </button>
      </Show>

      <Show when={error()}>
        <div class="change-set__conflicts">
          <p class="change-set__conflicts-head">Не удалось отменить: {error()}</p>
        </div>
      </Show>

      <Show when={conflicts().length > 0}>
        <div class="change-set__conflicts">
          <p class="change-set__conflicts-head">
            Не удалось отменить: эти файлы изменились после правок агента.
          </p>
          <For each={conflicts()}>
            {(conflict) => (
              <div class="change-set__conflict">{conflict.path}</div>
            )}
          </For>
        </div>
      </Show>

      <Show when={hasActive()}>
        <div class="change-set__actions">
          <button
            class="change-set__btn change-set__btn--danger"
            type="button"
            disabled={reverting()}
            onClick={() => void revertAll()}
          >
            {reverting()
              ? "Отмена…"
              : sets().length > 1
                ? "Отменить всё"
                : "Отменить"}
          </button>
        </div>
      </Show>
    </div>
  );
}
