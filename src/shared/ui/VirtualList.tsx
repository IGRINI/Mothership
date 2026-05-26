import { createEffect, For, JSX, Show } from "solid-js";

// NOTE: this used to wrap @tanstack/solid-virtual. That virtualizer drove an
// unterminating measure -> re-render -> re-measure loop plus a self-rescheduling
// scroll-reconcile rAF (confirmed by WebView2 traces: continuous serviceScriptedAnimations
// + long 56-65ms blocking tasks, vs 6.8ms for the same content under plain native
// scroll). For the list sizes this app actually has, virtualization was pure overhead
// and the source of the freeze. This is now a plain native-scroll list with optional
// stick-to-bottom. The public API is unchanged so callers don't change; `estimateSize`
// and `overscan` are accepted but ignored. Reintroduce real virtualization later,
// behind this same API, only if/when histories grow large enough to need it.
export interface VirtualListProps<TItem> {
  ariaLabel: string;
  class?: string;
  empty?: JSX.Element;
  estimateSize?: number | ((item: TItem, index: number) => number);
  getItemKey?: (item: TItem, index: number) => string | number;
  items: readonly TItem[];
  overscan?: number;
  stickToEnd?: boolean;
  children: (item: TItem, index: number) => JSX.Element;
}

export function VirtualList<TItem>(props: VirtualListProps<TItem>) {
  let scrollElement: HTMLDivElement | undefined;

  // Stick to the bottom when the list grows (chat behavior), but only on the first
  // population or when the user is already near the bottom — so it never yanks them
  // up while they read history. This reads layout once per count change (a new
  // message), never per scroll/frame, so it is not a hot path.
  let lastCount = 0;
  createEffect(() => {
    const count = props.items.length;
    const previous = lastCount;
    lastCount = count;

    if (!props.stickToEnd || !scrollElement || count <= previous) {
      return;
    }

    const element = scrollElement;
    const wasEmpty = previous === 0;
    const distanceFromBottom =
      element.scrollHeight - element.scrollTop - element.clientHeight;

    if (wasEmpty || distanceFromBottom < 160) {
      requestAnimationFrame(() => {
        element.scrollTop = element.scrollHeight;
      });
    }
  });

  return (
    <div
      ref={scrollElement}
      class={`virtual-list ${props.class ?? ""}`}
      role="list"
      aria-label={props.ariaLabel}
    >
      <Show when={props.items.length > 0} fallback={props.empty}>
        <For each={props.items}>
          {(item, index) => (
            <div class="virtual-list__row" role="listitem">
              {props.children(item, index())}
            </div>
          )}
        </For>
      </Show>
    </div>
  );
}
