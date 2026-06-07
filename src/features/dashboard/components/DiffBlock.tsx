import { For, Show, createEffect, createSignal, onCleanup } from "solid-js";

import { parseUnifiedHunks, type DiffRow } from "./diff";

// When the whole file is shown, leave this much room above the first change so a
// line or two of context stays visible rather than pinning it to the very top.
const FIRST_CHANGE_TOP_MARGIN_PX = 28;

/**
 * A unified-diff viewer with a double line-number gutter (old / new) and tone
 * coloring (+ green / − red / context / hunk). Pass either a raw unified `diff`
 * string or pre-parsed `rows`.
 *
 * - `maxRows`: cap the preview to N rows with a "Показать весь файл" reveal
 *   (used by the tool cards). Omit it to render every row (the change journal,
 *   which has its own whole-file control).
 * - `expanded`: the whole file is shown — jump the viewport to the first change
 *   so the edit isn't lost at the top of a long file.
 */
export function DiffBlock(props: {
  diff?: string;
  rows?: DiffRow[];
  expanded?: boolean;
  maxRows?: number;
}) {
  const [showAll, setShowAll] = createSignal(false);
  const rows = () => props.rows ?? parseUnifiedHunks(props.diff ?? "");
  const revealAll = () =>
    props.maxRows === undefined || props.expanded || showAll();
  const visibleRows = () => {
    const all = rows();
    return revealAll() ? all : all.slice(0, props.maxRows);
  };
  const hidden = () =>
    revealAll() ? 0 : Math.max(0, rows().length - (props.maxRows ?? 0));

  let bodyRef: HTMLDivElement | undefined;
  let scrollFrame = 0;

  // In whole-file mode, scroll the body to the first added/removed line so the
  // user lands on the edit. Re-runs when it expands or the rows change; the rAF
  // waits for the new rows to lay out before measuring.
  createEffect(() => {
    const expanded = props.expanded;
    void rows();
    if (!expanded) {
      return;
    }
    cancelAnimationFrame(scrollFrame);
    scrollFrame = requestAnimationFrame(() => {
      if (!bodyRef) {
        return;
      }
      const target = bodyRef.querySelector<HTMLElement>(
        ".diff-line--add, .diff-line--del",
      );
      if (!target) {
        return;
      }
      const delta =
        target.getBoundingClientRect().top - bodyRef.getBoundingClientRect().top;
      bodyRef.scrollTop = Math.max(
        0,
        bodyRef.scrollTop + delta - FIRST_CHANGE_TOP_MARGIN_PX,
      );
    });
  });

  onCleanup(() => cancelAnimationFrame(scrollFrame));

  return (
    <div class="diff-block">
      <div class="diff-block__body" ref={(el) => (bodyRef = el)}>
        {/* One inner box sized to the widest line so every row fills the same
            width — tone backgrounds then extend the full line on horizontal
            scroll instead of ending under the text. */}
        <div class="diff-block__lines">
          <For each={visibleRows()}>
            {(row) => (
              <div
                classList={{
                  "diff-line": true,
                  [`diff-line--${row.tone}`]: true,
                }}
              >
                <span class="diff-line__gutter">
                  <i>{row.oldNo ?? ""}</i>
                  <i>{row.newNo ?? ""}</i>
                </span>
                <span class="diff-line__text">
                  {row.text.length > 0 ? row.text : " "}
                </span>
              </div>
            )}
          </For>
        </div>
      </div>
      <Show when={hidden() > 0}>
        <button
          class="diff-block__more"
          type="button"
          onClick={() => setShowAll(true)}
        >
          Показать весь файл
        </button>
      </Show>
    </div>
  );
}
