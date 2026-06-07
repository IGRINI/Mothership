import { For, Show, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { Portal } from "solid-js/web";

export interface ContextMenuItem {
  label: string;
  onSelect: () => void;
  danger?: boolean;
}

interface ContextMenuState {
  x: number;
  y: number;
  items: ContextMenuItem[];
}

// A single app-wide custom context menu. Any element opens it from its
// `contextmenu` handler via `openContextMenu`; one `<ContextMenu />` instance
// (mounted at the app root) renders it. Custom (not the OS-native Tauri menu) so
// it matches the app theme and needs no menu capabilities.
const [state, setState] = createSignal<ContextMenuState | null>(null);

export function openContextMenu(next: ContextMenuState) {
  setState(next);
}

export function closeContextMenu() {
  setState(null);
}

export function ContextMenu() {
  let ref: HTMLDivElement | undefined;

  onMount(() => {
    const onPointerDown = (event: PointerEvent) => {
      if (ref && event.target instanceof Node && !ref.contains(event.target)) {
        closeContextMenu();
      }
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        closeContextMenu();
      }
    };
    const onAway = () => closeContextMenu();

    // Capture phase: catch the pointer-down before it reaches app handlers.
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("keydown", onKeyDown);
    window.addEventListener("resize", onAway);
    window.addEventListener("scroll", onAway, true);
    window.addEventListener("blur", onAway);
    onCleanup(() => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("resize", onAway);
      window.removeEventListener("scroll", onAway, true);
      window.removeEventListener("blur", onAway);
    });
  });

  // Keep the menu on-screen: after it renders, nudge it back inside the viewport.
  createEffect(() => {
    const current = state();
    if (!current) {
      return;
    }
    requestAnimationFrame(() => {
      if (!ref) {
        return;
      }
      const rect = ref.getBoundingClientRect();
      let x = current.x;
      let y = current.y;
      if (rect.right > window.innerWidth) {
        x = Math.max(4, window.innerWidth - rect.width - 4);
      }
      if (rect.bottom > window.innerHeight) {
        y = Math.max(4, window.innerHeight - rect.height - 4);
      }
      ref.style.left = `${x}px`;
      ref.style.top = `${y}px`;
    });
  });

  return (
    <Show when={state()}>
      {(current) => (
        <Portal>
          <div
            ref={ref}
            class="context-menu"
            role="menu"
            style={{ left: `${current().x}px`, top: `${current().y}px` }}
          >
            <For each={current().items}>
              {(item) => (
                <button
                  class="context-menu__item"
                  classList={{ "context-menu__item--danger": item.danger }}
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    closeContextMenu();
                    item.onSelect();
                  }}
                >
                  {item.label}
                </button>
              )}
            </For>
          </div>
        </Portal>
      )}
    </Show>
  );
}
