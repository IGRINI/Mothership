import { createEffect, createMemo, For, JSX, onCleanup, Show } from "solid-js";
import {
  createVirtualizer,
  type VirtualItem,
  type Virtualizer,
} from "@tanstack/solid-virtual";

type VirtualListKey = string | number | bigint;
type VirtualListScrollBehavior = "auto" | "smooth" | "instant";

export interface VirtualListProps<TItem> {
  ariaLabel: string;
  adjustScrollOnItemResize?: boolean;
  class?: string;
  empty?: JSX.Element;
  estimateSize?: number | ((item: TItem, index: number) => number);
  getItemKey?: (item: TItem, index: number) => VirtualListKey;
  items: readonly TItem[];
  overscan?: number;
  paddingEnd?: number;
  paddingStart?: number;
  scrollRef?: (element: HTMLDivElement | undefined) => void;
  stickToEnd?: boolean | VirtualListScrollBehavior;
  stickToEndThreshold?: number;
  children: (item: TItem, index: number) => JSX.Element;
}

// How long (ms) scroll events after our own follow-scroll are attributed to the
// follow movement instead of the user. Within this window a scroll event may
// confirm the pin but never break it — the user's wheel gets its own listener.
const PROGRAMMATIC_SCROLL_WINDOW_MS = 150;

export function VirtualList<TItem>(props: VirtualListProps<TItem>) {
  let scrollElement: HTMLDivElement | undefined;
  let pinnedToEnd = true;
  let followEndFrame = 0;
  let programmaticScrollUntil = 0;

  onCleanup(() => {
    cancelAnimationFrame(followEndFrame);
    props.scrollRef?.(undefined);
  });

  // Solid's <For> is keyed by *object reference*. Callers that derive their items
  // (e.g. a conversation timeline rebuilt from messages + tool calls) hand us a
  // fresh array of fresh objects on every state change, so a naive <For> would
  // dispose and recreate every row — re-instantiating each MessageRow and
  // re-parsing each markdown body — even when only one row actually changed.
  //
  // `getItemKey` lets us keep identity stable: an incoming item that is
  // shallow-equal to the one previously rendered under the same key is swapped for
  // its previous reference. <For> then reuses the existing DOM for every unchanged
  // row and only (re)builds the rows whose content really changed. When EVERY item
  // swaps stable (the common streaming case: rows read live state through by-id
  // maps and the items themselves are unchanged), the previous ARRAY is returned
  // so the memo doesn't notify at all and no row work happens.
  let previousByKey = new Map<VirtualListKey, TItem>();
  let previousItems: readonly TItem[] | undefined;
  const stableItems = createMemo<readonly TItem[]>(() => {
    const getKey = props.getItemKey;
    const items = props.items;
    if (!getKey) {
      previousByKey = new Map();
      previousItems = items;
      return items;
    }

    const nextByKey = new Map<VirtualListKey, TItem>();
    let unchanged =
      previousItems !== undefined && previousItems.length === items.length;
    const result = items.map((item, index) => {
      const key = getKey(item, index);
      const previous = previousByKey.get(key);
      const stable =
        previous !== undefined && shallowEqualItem(previous, item)
          ? previous
          : item;
      nextByKey.set(key, stable);
      if (unchanged && previousItems![index] !== stable) {
        unchanged = false;
      }
      return stable;
    });
    previousByKey = nextByKey;
    if (unchanged) {
      return previousItems!;
    }
    previousItems = result;
    return result;
  });

  const followOnAppend = () => {
    if (!props.stickToEnd) {
      return false;
    }

    return props.stickToEnd === true ? "auto" : props.stickToEnd;
  };

  const endThreshold = () => props.stickToEndThreshold ?? 24;

  // Pin/unpin is driven by USER intent only:
  //  - a wheel-up (or scroll that lands away from the end outside the
  //    programmatic window) unpins immediately — the follow loop stops fighting
  //    the user, which is exactly the "yanks me back down" failure mode;
  //  - scrolling back to within the threshold of the end re-pins.
  // Scroll events caused by our own follow-scroll fall inside the programmatic
  // window and may only confirm the pin, never flip it.
  const handleWheel = (event: WheelEvent) => {
    if (!props.stickToEnd) {
      return;
    }
    if (event.deltaY < 0) {
      pinnedToEnd = false;
      cancelAnimationFrame(followEndFrame);
      followEndFrame = 0;
    }
  };

  const handleScroll = () => {
    if (!scrollElement) {
      return;
    }
    resetHorizontalScroll(scrollElement);
    if (!props.stickToEnd) {
      pinnedToEnd = false;
      return;
    }

    const distance = distanceFromEnd(scrollElement);
    if (performance.now() <= programmaticScrollUntil) {
      if (distance <= endThreshold()) {
        // Our follow-scroll landed at the end; leave the window early so the
        // user's next gesture is attributed to them.
        programmaticScrollUntil = 0;
      }
      return;
    }
    pinnedToEnd = distance <= endThreshold();
  };

  const updatePinnedToEnd = () => {
    if (!props.stickToEnd || !scrollElement) {
      pinnedToEnd = false;
      return;
    }

    resetHorizontalScroll(scrollElement);
    pinnedToEnd = distanceFromEnd(scrollElement) <= endThreshold();
  };

  const schedulePinnedEndScroll = (
    instance: Virtualizer<HTMLDivElement, HTMLDivElement>,
  ) => {
    if (!props.stickToEnd || !pinnedToEnd || !scrollElement) {
      return;
    }

    if (distanceFromEnd(scrollElement) <= endThreshold()) {
      return;
    }

    cancelAnimationFrame(followEndFrame);
    followEndFrame = requestAnimationFrame(() => {
      followEndFrame = 0;
      if (!props.stickToEnd || !pinnedToEnd || !scrollElement) {
        return;
      }

      programmaticScrollUntil =
        performance.now() + PROGRAMMATIC_SCROLL_WINDOW_MS;
      instance.scrollToEnd({ behavior: followOnAppend() || "auto" });
    });
  };

  const virtualizer = createVirtualizer<HTMLDivElement, HTMLDivElement>({
    get count() {
      return stableItems().length;
    },
    getScrollElement: () => scrollElement ?? null,
    estimateSize: (index) => {
      const estimate = props.estimateSize ?? 48;
      if (typeof estimate === "number") {
        return estimate;
      }

      return estimate(stableItems()[index]!, index);
    },
    getItemKey: (index) => {
      const item = stableItems()[index];
      return item && props.getItemKey ? props.getItemKey(item, index) : index;
    },
    get overscan() {
      return props.overscan ?? 4;
    },
    get paddingEnd() {
      return props.paddingEnd ?? 0;
    },
    get paddingStart() {
      return props.paddingStart ?? 0;
    },
    get anchorTo() {
      return props.stickToEnd ? "end" : "start";
    },
    // The follow behavior is owned entirely by schedulePinnedEndScroll above:
    // TanStack's own append-follow is disabled so exactly ONE mechanism moves
    // the scroll position (two competing followers caused the bottom "yank"
    // while the user scrolled up during streaming).
    followOnAppend: false,
    get scrollEndThreshold() {
      return endThreshold();
    },
    onChange: (instance) => {
      schedulePinnedEndScroll(instance);
    },
    useAnimationFrameWithResizeObserver: true,
  });

  createEffect(() => {
    const mode = props.adjustScrollOnItemResize;
    // Default ("smart"): while the user reads history (unpinned), resizing rows
    // above the viewport must not shift what they're looking at; while pinned
    // to the end the follow loop owns the position and adjustment would fight
    // it. `true` forces TanStack's default-on behavior, `false` forces off.
    virtualizer.shouldAdjustScrollPositionOnItemSizeChange =
      mode === false
        ? () => false
        : mode === true
          ? undefined
          : () => !pinnedToEnd;
  });

  // TanStack rebuilds VirtualItem objects whenever ANY measurement changes —
  // during streaming the growing row gets a fresh object every delta, which
  // would make <For> tear down and remount that row (and its markdown) per
  // token. Swap each incoming VirtualItem for the previous object with the same
  // key+index: rows only care about key/index (offsets are read separately
  // below), so identity survives pure size changes and <For> keeps the DOM.
  let previousRowsByKey = new Map<VirtualListKey, VirtualItem>();
  let previousRows: VirtualItem[] | undefined;
  const rowVirtualItems = createMemo<VirtualItem[]>(() => {
    const incoming = virtualizer.getVirtualItems();
    const nextByKey = new Map<VirtualListKey, VirtualItem>();
    let unchanged =
      previousRows !== undefined && previousRows.length === incoming.length;
    const rows = incoming.map((virtualItem, position) => {
      const key = virtualItem.key as VirtualListKey;
      const previous = previousRowsByKey.get(key);
      const stable =
        previous !== undefined && previous.index === virtualItem.index
          ? previous
          : virtualItem;
      nextByKey.set(key, stable);
      if (unchanged && previousRows![position] !== stable) {
        unchanged = false;
      }
      return stable;
    });
    previousRowsByKey = nextByKey;
    if (unchanged) {
      return previousRows!;
    }
    previousRows = rows;
    return rows;
  });

  // Offsets are intentionally read from the LIVE virtual items (not the stable
  // wrappers), so the window keeps translating while row identity stays fixed.
  const windowOffset = createMemo(
    () => virtualizer.getVirtualItems()[0]?.start ?? 0,
  );

  const rowItem = (index: number) => stableItems()[index];

  const measureRow = (element: HTMLDivElement, index: number) => {
    // TanStack reads the row index from `data-index` when measuring. In Solid the
    // ref can fire before the reactive `data-index` attribute is applied, so write
    // it synchronously here first — otherwise measureElement logs "Missing
    // attribute name 'data-index'" and skips measuring the row.
    element.setAttribute("data-index", String(index));
    virtualizer.measureElement(element);
  };

  const setScrollElement = (element: HTMLDivElement) => {
    scrollElement = element;
    resetHorizontalScroll(element);
    updatePinnedToEnd();
    props.scrollRef?.(element);
  };

  onCleanup(() => {
    scrollElement = undefined;
  });

  return (
    <div
      ref={setScrollElement}
      class={`virtual-list ${props.class ?? ""}`}
      role="list"
      aria-label={props.ariaLabel}
      onScroll={handleScroll}
      onWheel={handleWheel}
    >
      <Show when={stableItems().length > 0} fallback={props.empty}>
        <div
          class="virtual-list__spacer"
          style={{ height: `${virtualizer.getTotalSize()}px` }}
        >
          <div
            class="virtual-list__window"
            style={{ transform: `translateY(${windowOffset()}px)` }}
          >
            <For each={rowVirtualItems()}>
              {(virtualItem) => (
                <div
                  ref={(element) => measureRow(element, virtualItem.index)}
                  class="virtual-list__row"
                  data-index={virtualItem.index}
                  role="listitem"
                >
                  {props.children(
                    rowItem(virtualItem.index)!,
                    virtualItem.index,
                  )}
                </div>
              )}
            </For>
          </div>
        </div>
      </Show>
    </div>
  );
}

function distanceFromEnd(element: HTMLDivElement): number {
  return Math.max(
    0,
    element.scrollHeight - element.clientHeight - element.scrollTop,
  );
}

function resetHorizontalScroll(element: HTMLDivElement) {
  if (element.scrollLeft !== 0) {
    element.scrollLeft = 0;
  }
}

// Shallow structural equality for list items. Two items are treated as the same
// row when every own field matches by reference, which is exactly the signal we
// need: upstream state updates replace changed messages/tool calls with new
// objects but leave untouched ones referentially stable.
function shallowEqualItem(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) {
    return true;
  }
  if (
    typeof a !== "object" ||
    typeof b !== "object" ||
    a === null ||
    b === null
  ) {
    return false;
  }

  const aRecord = a as Record<string, unknown>;
  const bRecord = b as Record<string, unknown>;
  const aKeys = Object.keys(aRecord);
  if (aKeys.length !== Object.keys(bRecord).length) {
    return false;
  }

  for (const key of aKeys) {
    if (!Object.is(aRecord[key], bRecord[key])) {
      return false;
    }
  }
  return true;
}
