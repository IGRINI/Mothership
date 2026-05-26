import { createEffect, For, JSX, Show } from "solid-js";
import { createVirtualizer } from "@tanstack/solid-virtual";

export interface VirtualListProps<TItem> {
  ariaLabel: string;
  class?: string;
  empty?: JSX.Element;
  estimateSize: number | ((item: TItem, index: number) => number);
  getItemKey?: (item: TItem, index: number) => string | number;
  items: readonly TItem[];
  overscan?: number;
  stickToEnd?: boolean;
  children: (item: TItem, index: number) => JSX.Element;
}

export function VirtualList<TItem>(props: VirtualListProps<TItem>) {
  let scrollElement: HTMLDivElement | undefined;

  const virtualizer = createVirtualizer<HTMLDivElement, HTMLDivElement>({
    get count() {
      return props.items.length;
    },
    estimateSize: (index) => {
      const item = props.items[index];
      if (typeof props.estimateSize === "function") {
        return item === undefined ? 1 : props.estimateSize(item, index);
      }

      return props.estimateSize;
    },
    getScrollElement: () => scrollElement ?? null,
    getItemKey: (index) => {
      const item = props.items[index];
      return item === undefined
        ? index
        : props.getItemKey?.(item, index) ?? index;
    },
    measureElement: (element) => element.getBoundingClientRect().height,
    overscan: props.overscan ?? 12,
  });

  createEffect(() => {
    const itemCount = props.items.length;
    const lastItem = props.items[itemCount - 1];
    if (!props.stickToEnd || itemCount === 0) {
      return;
    }

    void lastItem;
    requestAnimationFrame(() => {
      virtualizer.measure();
      virtualizer.scrollToIndex(itemCount - 1, { align: "end" });
    });
  });

  const virtualItems = () => virtualizer.getVirtualItems();
  const firstVirtualStart = () => virtualItems()[0]?.start ?? 0;

  return (
    <div
      ref={scrollElement}
      class={`virtual-list ${props.class ?? ""}`}
      role="list"
      aria-label={props.ariaLabel}
    >
      <Show when={props.items.length > 0} fallback={props.empty}>
        <div
          class="virtual-list__spacer"
          style={{ height: `${virtualizer.getTotalSize()}px` }}
        >
          <div
            class="virtual-list__window"
            style={{
              transform: `translateY(${firstVirtualStart()}px)`,
            }}
          >
            <For each={virtualItems()}>
              {(virtualRow) => {
                const item = () => props.items[virtualRow.index];

                return (
                  <Show
                    when={item()}
                    keyed
                    fallback={null}
                  >
                    {(currentItem) => (
                      <div
                        ref={(element) => virtualizer.measureElement(element)}
                        class="virtual-list__row"
                        data-index={virtualRow.index}
                        role="listitem"
                      >
                        {props.children(currentItem, virtualRow.index)}
                      </div>
                    )}
                  </Show>
                );
              }}
            </For>
          </div>
        </div>
      </Show>
    </div>
  );
}
