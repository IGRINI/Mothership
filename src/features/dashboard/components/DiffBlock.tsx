import { For, Show, createSignal } from "solid-js";

import { parseUnifiedHunks, type DiffRow } from "./diff";

const VISIBLE_CAP = 10;

/**
 * A unified-diff viewer with a double line-number gutter (old / new), tone
 * coloring (+ green / − red / context / hunk), and a 10-line cap with a
 * "ещё N строк" reveal. Pass either a raw unified `diff` string or pre-parsed
 * `rows`.
 */
export function DiffBlock(props: { diff?: string; rows?: DiffRow[] }) {
  const [showAll, setShowAll] = createSignal(false);
  const rows = () => props.rows ?? parseUnifiedHunks(props.diff ?? "");
  const visibleRows = () =>
    showAll() ? rows() : rows().slice(0, VISIBLE_CAP);
  const hidden = () =>
    showAll() ? 0 : Math.max(0, rows().length - VISIBLE_CAP);

  return (
    <div class="diff-block">
      <div class="diff-block__body">
        <For each={visibleRows()}>
          {(row) => (
            <div classList={{ "diff-line": true, [`diff-line--${row.tone}`]: true }}>
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
      <Show when={hidden() > 0}>
        <button
          class="diff-block__more"
          type="button"
          onClick={() => setShowAll(true)}
        >
          … ещё {hidden()} строк
        </button>
      </Show>
    </div>
  );
}
