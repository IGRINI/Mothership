import { For, Show, createSignal } from "solid-js";
import { ExternalLink, FileText } from "lucide-solid";

import { highlightCode } from "./highlight";
import type { CodeLineModel } from "./code";

const VISIBLE_CAP = 10;

/**
 * A line-numbered code viewer with a self-written highlighter, a read-region
 * highlight, a 10-line preview cap, and an optional "Открыть" link.
 *
 * The preview shows the first {@link VISIBLE_CAP} lines; "Раскрыть весь файл"
 * reveals the rest. Lazy loading: the component renders ONLY the lines it is
 * handed. When more content exists beyond what is loaded, the host passes
 * `onLoadMore` (e.g. a Core artifact-range query) — expanding then also fetches
 * the rest. Nothing past the window is pulled into memory until the user asks.
 */
export function CodeBlock(props: {
  lines: CodeLineModel[];
  title?: string;
  path?: string;
  onOpen?: (path: string) => void;
  /** Called when the user asks for content beyond what is loaded. */
  onLoadMore?: () => void;
  /** Label for the lazy-load button once expanded (defaults to "Загрузить ещё"). */
  loadMoreLabel?: string;
  /** True while a lazy load is in flight. */
  loading?: boolean;
  /** True once everything is loaded — hides the lazy-load button. */
  atEnd?: boolean;
}) {
  const [showAll, setShowAll] = createSignal(false);
  const visibleLines = () =>
    showAll() ? props.lines : props.lines.slice(0, VISIBLE_CAP);
  const hiddenLoaded = () =>
    showAll() ? 0 : Math.max(0, props.lines.length - VISIBLE_CAP);
  // More content can be lazily fetched (the file is larger than the loaded window).
  const canLazyLoad = () => Boolean(props.onLoadMore) && !props.atEnd;
  // Collapsed and there is anything more to reveal (hidden loaded lines or more
  // to fetch) — show the single "Раскрыть весь файл" button.
  const collapsedHasMore = () =>
    !showAll() && (hiddenLoaded() > 0 || canLazyLoad());

  // Reveal the loaded lines and, if the file is larger than the window, kick off
  // the lazy fetch of the rest.
  const expand = () => {
    setShowAll(true);
    if (canLazyLoad()) {
      props.onLoadMore?.();
    }
  };

  return (
    <div class="code-block">
      <Show when={props.title ?? props.path}>
        <div class="code-block__head">
          <FileText size={13} />
          <span class="code-block__title">{props.title ?? props.path}</span>
          <Show when={props.path && props.onOpen}>
            <button
              class="code-block__open"
              type="button"
              title="Открыть во внешнем приложении"
              onClick={() => props.onOpen?.(props.path as string)}
            >
              <ExternalLink size={12} />
              Открыть
            </button>
          </Show>
        </div>
      </Show>

      <div class="code-block__code">
        <For each={visibleLines()}>
          {(line) => (
            <div
              classList={{
                "code-line": true,
                "code-line--read": Boolean(line.read),
                "code-line--ctx": Boolean(line.context),
              }}
            >
              <span class="code-line__no">{line.no}</span>
              <code class="code-line__text">
                <CodeTokens text={line.text} />
              </code>
            </div>
          )}
        </For>
      </div>

      <Show when={collapsedHasMore()}>
        <button class="code-block__more" type="button" onClick={expand}>
          Раскрыть весь файл
        </button>
      </Show>
      <Show when={showAll() && canLazyLoad()}>
        <button
          class="code-block__more"
          type="button"
          disabled={props.loading}
          onClick={() => props.onLoadMore?.()}
        >
          {props.loading ? "загрузка…" : props.loadMoreLabel ?? "Загрузить ещё"}
        </button>
      </Show>
    </div>
  );
}

function CodeTokens(props: { text: string }) {
  const tokens = () =>
    props.text.length > 0 ? highlightCode(props.text) : [{ cls: "", text: " " }];
  return (
    <For each={tokens()}>
      {(token) => <span class={token.cls || undefined}>{token.text}</span>}
    </For>
  );
}
