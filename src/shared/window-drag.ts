import { getCurrentWindow } from "@tauri-apps/api/window";

// Resolve the Tauri window lazily and only inside the Tauri runtime. Outside
// Tauri (plain browser preview) this returns null and the handlers no-op.
let cachedWindow: ReturnType<typeof getCurrentWindow> | null | undefined;
function appWindow() {
  if (cachedWindow === undefined) {
    cachedWindow =
      typeof window !== "undefined" && "__TAURI_INTERNALS__" in window
        ? getCurrentWindow()
        : null;
  }
  return cachedWindow;
}

// Controls inside a drag area that must stay clickable rather than start a drag.
const INTERACTIVE_SELECTOR =
  "button, a, input, select, textarea, label, kbd, [role='button'], [contenteditable='true'], .no-window-drag";

/**
 * Starts a native window drag from a custom (frameless) title area using JS.
 *
 * We deliberately do NOT use the CSS `-webkit-app-region: drag` non-client
 * regions: on Windows/WebView2 those force the whole page into a single
 * compositor layer, so every scroll repaints the entire window and the UI feels
 * frozen. Driving the drag from JS keeps the page composited.
 *
 * Clicks that land on an interactive control are ignored so buttons/selects keep
 * working; a double-click toggles maximize, matching native title-bar behavior.
 */
export function startWindowDrag(event: MouseEvent) {
  if (event.button !== 0) {
    return;
  }

  const target = event.target as HTMLElement | null;
  if (target?.closest(INTERACTIVE_SELECTOR)) {
    return;
  }

  const win = appWindow();
  if (!win) {
    return;
  }

  if (event.detail === 2) {
    void win.toggleMaximize();
    return;
  }

  void win.startDragging();
}
