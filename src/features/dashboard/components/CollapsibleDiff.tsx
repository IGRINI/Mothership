import { For, Show, createMemo, createSignal } from "solid-js";

import { parseUnifiedHunks, type DiffRow } from "./diff";

// Lines of real context kept around every change; longer unchanged runs collapse
// into a single clickable "unfold" row that reveals the whole run.
const CONTEXT = 3;

function DiffLine(props: { row: DiffRow }) {
  return (
    <div
      classList={{ "diff-line": true, [`diff-line--${props.row.tone}`]: true }}
    >
      <span class="diff-line__gutter">
        <i>{props.row.oldNo ?? ""}</i>
        <i>{props.row.newNo ?? ""}</i>
      </span>
      <span class="diff-line__text">
        {props.row.text.length > 0 ? props.row.text : " "}
      </span>
    </div>
  );
}

/**
 * One collapsed run of unchanged lines between/around changes. Hidden by default;
 * ↑/↓ reveal EXPAND_STEP lines from either end (toward the previous / next
 * change), or a single control reveals the whole run when it's small — the
 * GitHub "unfold" interaction. Holds its own reveal state.
 */
function GapSegment(props: { rows: DiffRow[]; start: number; end: number }) {
  const [shown, setShown] = createSignal(false);
  const total = props.end - props.start;
  const indices = () => {
    const out: number[] = [];
    for (let i = props.start; i < props.end; i++) out.push(i);
    return out;
  };

  return (
    <Show
      when={shown()}
      fallback={
        <div class="diff-expander">
          <button
            class="diff-expander__btn diff-expander__btn--all"
            type="button"
            onClick={() => setShown(true)}
          >
            ⋯ Показать {total} скрытых строк
          </button>
        </div>
      }
    >
      <For each={indices()}>{(i) => <DiffLine row={props.rows[i]} />}</For>
    </Show>
  );
}

/**
 * GitHub-style diff: render the changes with a few lines of context and collapse
 * every longer unchanged run into an "unfold" row. Feed it the WHOLE-file diff
 * (real line numbers) — the collapsing happens client-side, so expanding is
 * instant with no extra round-trip.
 */
export function CollapsibleDiff(props: { diff?: string; rows?: DiffRow[] }) {
  const content = createMemo(() =>
    (props.rows ?? parseUnifiedHunks(props.diff ?? "")).filter(
      (row) => row.tone === "add" || row.tone === "del" || row.tone === "ctx",
    ),
  );

  // Split into shown rows (changes + their context) and collapsible gaps.
  const segments = createMemo(() => {
    const rows = content();
    const keep = new Array<boolean>(rows.length).fill(false);
    rows.forEach((row, i) => {
      if (row.tone === "add" || row.tone === "del") {
        const lo = Math.max(0, i - CONTEXT);
        const hi = Math.min(rows.length - 1, i + CONTEXT);
        for (let j = lo; j <= hi; j++) keep[j] = true;
      }
    });

    const segs: Array<
      | { kind: "row"; index: number }
      | { kind: "gap"; start: number; end: number }
    > = [];
    let i = 0;
    while (i < rows.length) {
      if (keep[i]) {
        segs.push({ kind: "row", index: i });
        i++;
      } else {
        const start = i;
        while (i < rows.length && !keep[i]) i++;
        segs.push({ kind: "gap", start, end: i });
      }
    }
    return segs;
  });

  return (
    <div class="diff-block">
      <div class="diff-block__body">
        <div class="diff-block__lines">
          <For each={segments()}>
            {(seg) =>
              seg.kind === "row" ? (
                <DiffLine row={content()[seg.index]} />
              ) : (
                <GapSegment rows={content()} start={seg.start} end={seg.end} />
              )
            }
          </For>
        </div>
      </div>
    </div>
  );
}
