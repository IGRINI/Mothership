import { createEffect, createMemo, For, JSX, onCleanup, Show } from "solid-js";
import { createVirtualizer, type Virtualizer } from "@tanstack/solid-virtual";

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

export function VirtualList<TItem>(props: VirtualListProps<TItem>) {
  let scrollElement: HTMLDivElement | undefined;
  let pinnedToEnd = true;
  let followEndFrame = 0;

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
  // row and only (re)builds the rows whose content really changed (e.g. the single
  // message currently streaming). Without a key we fall back to the raw items.
  let previousByKey = new Map<VirtualListKey, TItem>();
  const stableItems = createMemo<readonly TItem[]>(() => {
    const getKey = props.getItemKey;
    const items = props.items;
    if (!getKey) {
      previousByKey = new Map();
      return items;
    }

    const nextByKey = new Map<VirtualListKey, TItem>();
    const result = items.map((item, index) => {
      const key = getKey(item, index);
      const previous = previousByKey.get(key);
      const stable =
        previous !== undefined && shallowEqualItem(previous, item)
          ? previous
          : item;
      nextByKey.set(key, stable);
      return stable;
    });
    previousByKey = nextByKey;
    return result;
  });

  const followOnAppend = () => {
    if (!props.stickToEnd) {
      return false;
    }

    return props.stickToEnd === true ? "auto" : props.stickToEnd;
  };

  const endThreshold = () => props.stickToEndThreshold ?? 1;

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

      instance.scrollToEnd({ behavior: followOnAppend() || "auto" });
      pinnedToEnd = true;
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
    get followOnAppend() {
      return followOnAppend();
    },
    get scrollEndThreshold() {
      return props.stickToEndThreshold ?? 1;
    },
    onChange: (instance) => {
      schedulePinnedEndScroll(instance);
    },
    useAnimationFrameWithResizeObserver: true,
  });

  createEffect(() => {
    virtualizer.shouldAdjustScrollPositionOnItemSizeChange =
      props.adjustScrollOnItemResize === false ? () => false : undefined;
  });

  const virtualItems = createMemo(() => virtualizer.getVirtualItems());
  const windowOffset = createMemo(() => virtualItems()[0]?.start ?? 0);

  const rowItem = (index: number) => stableItems()[index];
  const rowIndex = (virtualIndex: number) => virtualIndex;

  const measureRow = (element: HTMLDivElement) => {
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
      onScroll={updatePinnedToEnd}
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
            <For each={virtualItems()}>
              {(virtualItem) => (
                <div
                  ref={measureRow}
                  class="virtual-list__row"
                  data-index={virtualItem.index}
                  role="listitem"
                >
                  {props.children(
                    rowItem(virtualItem.index)!,
                    rowIndex(virtualItem.index),
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
