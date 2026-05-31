import { createEffect, For, JSX, Show, onCleanup } from "solid-js";

// Plain native-scroll list (the previous @tanstack/solid-virtual wrapper drove a
// measure -> re-render -> re-measure loop that froze scrolling). Public API is
// unchanged; `estimateSize` and `overscan` are accepted but ignored.
//
// Scroll behaviour:
// - With `scrollKey` (e.g. a chat id): the scroll position is saved per key and
//   persisted to localStorage, so switching chats restores where you were and it
//   survives an app restart. A key seen for the first time opens at the bottom.
//   "Was at the bottom" is stored as a flag, so a bottom-anchored chat re-anchors
//   to the bottom. Restoration runs as a short multi-frame "pin" so it holds the
//   target while messages/markdown/avatars finish laying out (their height grows
//   asynchronously and would otherwise leave a single-shot restore near the top).
// - With `stickToEnd`: appended rows and streaming height growth stick to the
//   bottom only while the user is already at the bottom. A normal user scroll up
//   disables following until they return to the bottom.
export interface VirtualListProps<TItem> {
  ariaLabel: string;
  class?: string;
  empty?: JSX.Element;
  estimateSize?: number | ((item: TItem, index: number) => number);
  getItemKey?: (item: TItem, index: number) => string | number;
  items: readonly TItem[];
  overscan?: number;
  stickToEnd?: boolean;
  scrollKey?: string;
  children: (item: TItem, index: number) => JSX.Element;
}

const SCROLL_STORE_KEY = "mothership:chat-scroll-positions";
const FOLLOW_END_THRESHOLD_PX = 48;
const AT_BOTTOM_PX = 48;
const RESTORE_PIN_FRAMES = 12;

interface ScrollPos {
  top: number;
  atBottom: boolean;
}

function loadScrollPositions(): Record<string, ScrollPos> {
  try {
    const raw = JSON.parse(localStorage.getItem(SCROLL_STORE_KEY) ?? "{}") as Record<
      string,
      unknown
    >;
    const out: Record<string, ScrollPos> = {};
    for (const [key, value] of Object.entries(raw)) {
      if (typeof value === "number") {
        out[key] = { top: value, atBottom: false };
      } else if (
        value &&
        typeof value === "object" &&
        typeof (value as ScrollPos).top === "number"
      ) {
        out[key] = {
          top: (value as ScrollPos).top,
          atBottom: Boolean((value as ScrollPos).atBottom),
        };
      }
    }
    return out;
  } catch {
    return {};
  }
}

export function VirtualList<TItem>(props: VirtualListProps<TItem>) {
  let scrollElement: HTMLDivElement | undefined;
  const positions = loadScrollPositions();
  let restoring = false;
  let followEnd = true;
  let persistTimer: number | undefined;
  let pinRaf = 0;
  let followRaf = 0;
  let resizeObserver: ResizeObserver | undefined;
  let restoredForKey: string | undefined;
  let lastCount = 0;

  const persistSoon = () => {
    clearTimeout(persistTimer);
    persistTimer = window.setTimeout(() => {
      try {
        localStorage.setItem(SCROLL_STORE_KEY, JSON.stringify(positions));
      } catch {
        /* localStorage unavailable — ignore */
      }
    }, 400);
  };

  const handleScroll = () => {
    const key = props.scrollKey;
    const element = scrollElement;
    // Save only genuine user scrolls of the current, settled key: skip while
    // restoring, skip other keys, and skip when there is nothing scrollable (the
    // transient state right after a chat switch) so we never record a bogus 0.
    if (!element || restoring) return;

    const distance = distanceFromEnd(element);
    followEnd = distance <= FOLLOW_END_THRESHOLD_PX;

    if (!key || key !== restoredForKey) return;
    if (element.scrollHeight <= element.clientHeight + 4) return;
    positions[key] = {
      top: element.scrollTop,
      atBottom: distance <= AT_BOTTOM_PX,
    };
    persistSoon();
  };

  const attachContentElement = (element: HTMLDivElement) => {
    resizeObserver?.disconnect();

    if (typeof ResizeObserver === "undefined") {
      return;
    }

    resizeObserver = new ResizeObserver(() => {
      const scrollParent = scrollElement;
      if (!scrollParent || !props.stickToEnd || restoring || !followEnd) {
        return;
      }

      requestScrollToEnd(scrollParent);
    });
    resizeObserver.observe(element);
  };

  // Hold the target position across several frames so it survives async layout
  // growth (markdown/avatars) and the content swap when switching chats.
  const pinTo = (element: HTMLDivElement, target: ScrollPos | null) => {
    cancelAnimationFrame(pinRaf);
    followEnd = !target || target.atBottom;
    restoring = true;
    let frame = 0;
    const step = () => {
      if (!target || target.atBottom) {
        scrollToEnd(element);
      } else {
        element.scrollTop = target.top;
      }
      frame += 1;
      if (frame < RESTORE_PIN_FRAMES) {
        pinRaf = requestAnimationFrame(step);
      } else {
        restoring = false;
      }
    };
    pinRaf = requestAnimationFrame(step);
  };

  const requestScrollToEnd = (element: HTMLDivElement) => {
    cancelAnimationFrame(followRaf);
    scrollToEnd(element);
    followRaf = requestAnimationFrame(() => {
      scrollToEnd(element);
    });
  };

  const stickIfFollowingEnd = (element: HTMLDivElement) => {
    if (followEnd) {
      requestScrollToEnd(element);
    }
  };

  createEffect(() => {
    const key = props.scrollKey;
    const count = props.items.length;
    const element = scrollElement;
    if (!element) return;

    if (key != null) {
      if (key !== restoredForKey) {
        if (count === 0) {
          lastCount = count;
          return; // wait until this key has content to anchor against
        }
        restoredForKey = key;
        lastCount = count;
        pinTo(element, positions[key] ?? null);
        return;
      }

      if (props.stickToEnd && count > lastCount && !restoring) {
        requestAnimationFrame(() => stickIfFollowingEnd(element));
      }
      lastCount = count;
      return;
    }

    // Unkeyed (e.g. sidebar lists): optional stick-to-end only.
    if (props.stickToEnd && count > lastCount && count > 0) {
      requestAnimationFrame(() => stickIfFollowingEnd(element));
    }
    lastCount = count;
  });

  onCleanup(() => {
    cancelAnimationFrame(pinRaf);
    cancelAnimationFrame(followRaf);
    clearTimeout(persistTimer);
    resizeObserver?.disconnect();
  });

  return (
    <div
      ref={scrollElement}
      class={`virtual-list ${props.class ?? ""}`}
      role="list"
      aria-label={props.ariaLabel}
      onScroll={handleScroll}
    >
      <Show when={props.items.length > 0} fallback={props.empty}>
        <div ref={attachContentElement} class="virtual-list__content">
          <For each={props.items}>
            {(item, index) => (
              <div class="virtual-list__row" role="listitem">
                {props.children(item, index())}
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function distanceFromEnd(element: HTMLElement) {
  return Math.max(0, element.scrollHeight - element.scrollTop - element.clientHeight);
}

function scrollToEnd(element: HTMLElement) {
  element.scrollTop = element.scrollHeight;
}
