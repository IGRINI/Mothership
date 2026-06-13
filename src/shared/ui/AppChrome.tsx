import { createEffect, createSignal, JSX, onCleanup, onMount, Show } from "solid-js";
import {
  ArrowDown,
  ArrowUp,
  Search,
  Settings,
  Minus,
  Square,
  X,
} from "lucide-solid";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { AgentStatusPill } from "./AgentStatusPill";

// Resolve the Tauri window lazily and only inside the Tauri runtime. Calling
// getCurrentWindow() at module load reads window.__TAURI_INTERNALS__.metadata,
// which is undefined in a plain browser and throws — that would block the whole
// app from mounting, including its intended browser preview mode. Outside Tauri
// this returns null and the window controls become no-ops.
let cachedAppWindow: ReturnType<typeof getCurrentWindow> | null | undefined;
function appWindow() {
  if (cachedAppWindow === undefined) {
    cachedAppWindow =
      typeof window !== "undefined" && "__TAURI_INTERNALS__" in window
        ? getCurrentWindow()
        : null;
  }
  return cachedAppWindow;
}
const isMac = /Mac/i.test(navigator.platform);
const openFindEvent = "mothership-open-find";

type ResizeDirection =
  | "East"
  | "North"
  | "NorthEast"
  | "NorthWest"
  | "South"
  | "SouthEast"
  | "SouthWest"
  | "West";

declare global {
  interface Window {
    find?: (
      text: string,
      caseSensitive?: boolean,
      backwards?: boolean,
      wrapAround?: boolean,
      wholeWord?: boolean,
      searchInFrames?: boolean,
      showDialog?: boolean,
    ) => boolean;
  }
}

interface AppChromeProps {
  children: JSX.Element;
  onOpenSettings?: () => void;
}

export function AppChrome(props: AppChromeProps) {
  const search = createWindowSearchController();
  const isResizing = createResizeActivity();

  return (
    <div
      classList={{
        "app-frame": true,
        "app-frame--mac": isMac,
        "app-frame--resizing": isResizing(),
        "app-frame--windows": !isMac,
      }}
    >
      <TitleBar />
      <Show when={!isMac}>
        <WindowResizeHandles />
      </Show>
      <div class="app-frame__body">{props.children}</div>
      <Show when={search.isOpen()}>
        <WindowSearch
          matchCount={search.matchCount()}
          query={search.query()}
          onClose={search.close}
          onFindNext={search.findNext}
          onFindPrevious={search.findPrevious}
          onQueryChange={search.setQuery}
        />
      </Show>
      <StatusBar onOpenSettings={props.onOpenSettings} />
    </div>
  );
}

function WindowResizeHandles() {
  return (
    <div class="window-resize-handles" aria-hidden="true">
      <WindowResizeHandle direction="North" edge="north" />
      <WindowResizeHandle direction="East" edge="east" />
      <WindowResizeHandle direction="South" edge="south" />
      <WindowResizeHandle direction="West" edge="west" />
      <WindowResizeHandle direction="NorthWest" edge="north-west" />
      <WindowResizeHandle direction="NorthEast" edge="north-east" />
      <WindowResizeHandle direction="SouthEast" edge="south-east" />
      <WindowResizeHandle direction="SouthWest" edge="south-west" />
    </div>
  );
}

interface WindowResizeHandleProps {
  direction: ResizeDirection;
  edge: string;
}

function WindowResizeHandle(props: WindowResizeHandleProps) {
  const startResize = (event: MouseEvent) => {
    if (event.button !== 0) {
      return;
    }

    event.preventDefault();
    event.stopPropagation();
    void appWindow()?.startResizeDragging(props.direction);
  };

  return (
    <div
      class={`window-resize-handle window-resize-handle--${props.edge}`}
      onMouseDown={startResize}
    />
  );
}

function createResizeActivity() {
  const [isResizing, setIsResizing] = createSignal(false);
  let resizeTimeout: number | undefined;

  const markResizing = () => {
    if (!isResizing()) {
      setIsResizing(true);
    }

    if (resizeTimeout !== undefined) {
      window.clearTimeout(resizeTimeout);
    }

    resizeTimeout = window.setTimeout(() => {
      setIsResizing(false);
    }, 180);
  };

  onMount(() => {
    window.addEventListener("resize", markResizing, { passive: true });
  });

  onCleanup(() => {
    window.removeEventListener("resize", markResizing);

    if (resizeTimeout !== undefined) {
      window.clearTimeout(resizeTimeout);
    }
  });

  return isResizing;
}

function createWindowSearchController() {
  const [isOpen, setIsOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [matchCount, setMatchCount] = createSignal(0);

  const open = () => {
    setIsOpen(true);
    requestAnimationFrame(() => {
      const input = document.querySelector<HTMLInputElement>("[data-window-search-input]");
      input?.focus();
      input?.select();
    });
  };

  const close = () => {
    setIsOpen(false);
    setMatchCount(0);
    window.getSelection()?.removeAllRanges();
  };

  const find = (backwards = false) => {
    const value = query().trim();

    if (!value || typeof window.find !== "function") {
      return;
    }

    window.find(value, false, backwards, true, false, false, false);
  };

  const interceptNativeFind = (event: KeyboardEvent) => {
    const usesPlatformModifier = isMac ? event.metaKey : event.ctrlKey;
    const isFindShortcut =
      usesPlatformModifier &&
      !event.altKey &&
      !event.shiftKey &&
      (event.code === "KeyF" || event.key.toLowerCase() === "f");

    if (!isFindShortcut) {
      if (isOpen() && event.key === "Escape") {
        event.preventDefault();
        event.stopImmediatePropagation();
        close();
      }

      return;
    }

    event.preventDefault();
    event.stopImmediatePropagation();
    open();
  };

  let unlistenFindShortcut: (() => void) | undefined;

  onMount(() => {
    window.addEventListener("keydown", interceptNativeFind, { capture: true });

    void listen(openFindEvent, () => {
      open();
    })
      .then((unlisten) => {
        unlistenFindShortcut = unlisten;
      })
      .catch((error) => {
        console.error("failed to bind find shortcut event", error);
      });
  });

  onCleanup(() => {
    window.removeEventListener("keydown", interceptNativeFind, { capture: true });
    unlistenFindShortcut?.();
  });

  createEffect(() => {
    if (!isOpen()) {
      return;
    }

    const value = query().trim();
    setMatchCount(value ? countVisibleMatches(value) : 0);
  });

  return {
    close,
    findNext: () => find(false),
    findPrevious: () => find(true),
    isOpen,
    matchCount,
    query,
    setQuery,
  };
}

interface WindowSearchProps {
  matchCount: number;
  query: string;
  onClose: () => void;
  onFindNext: () => void;
  onFindPrevious: () => void;
  onQueryChange: (value: string) => void;
}

function WindowSearch(props: WindowSearchProps) {
  const handleKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Enter") {
      event.preventDefault();

      if (event.shiftKey) {
        props.onFindPrevious();
        return;
      }

      props.onFindNext();
    }
  };

  return (
    <section class="window-search" data-window-search data-search-excluded>
      <div class="window-search__field">
        <Search size={15} />
        <input
          data-window-search-input
          type="search"
          value={props.query}
          placeholder="Find in workspace"
          onInput={(event) => props.onQueryChange(event.currentTarget.value)}
          onKeyDown={handleKeyDown}
        />
      </div>
      <span class="window-search__count">{props.matchCount}</span>
      <button
        class="window-search__button"
        type="button"
        title="Previous match"
        aria-label="Previous match"
        onClick={props.onFindPrevious}
      >
        <ArrowUp size={15} />
      </button>
      <button
        class="window-search__button"
        type="button"
        title="Next match"
        aria-label="Next match"
        onClick={props.onFindNext}
      >
        <ArrowDown size={15} />
      </button>
      <button
        class="window-search__button window-search__button--close"
        type="button"
        title="Close search"
        aria-label="Close search"
        onClick={props.onClose}
      >
        <X size={16} />
      </button>
    </section>
  );
}

function countVisibleMatches(query: string) {
  const root = document.querySelector(".app-frame__body");

  if (!root) {
    return 0;
  }

  const needle = query.toLocaleLowerCase();
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      const text = node.textContent;
      const parent = node.parentElement;

      if (!text?.trim() || parent?.closest("[data-search-excluded]")) {
        return NodeFilter.FILTER_REJECT;
      }

      return NodeFilter.FILTER_ACCEPT;
    },
  });

  let total = 0;
  let current = walker.nextNode();

  while (current) {
    const haystack = current.textContent?.toLocaleLowerCase() ?? "";
    let index = haystack.indexOf(needle);

    while (index !== -1) {
      total += 1;
      index = haystack.indexOf(needle, index + needle.length);
    }

    current = walker.nextNode();
  }

  return total;
}

function TitleBar() {
  const startWindowDrag = (event: MouseEvent) => {
    if (event.buttons !== 1) {
      return;
    }

    if (event.detail === 2) {
      void appWindow()?.toggleMaximize();
      return;
    }

    void appWindow()?.startDragging();
  };

  return (
    <header
      classList={{
        "app-titlebar": true,
        "app-titlebar--mac": isMac,
        "app-titlebar--windows": !isMac,
      }}
    >
      <Show when={isMac}>
        <MacTrafficControls />
      </Show>

      <Show when={isMac}>
        <div
          class="app-titlebar__drag-zone"
          data-tauri-drag-region
          onMouseDown={startWindowDrag}
        />
      </Show>

      <Show when={!isMac}>
        <WindowControls />
      </Show>
    </header>
  );
}

function MacTrafficControls() {
  return (
    <div class="app-titlebar__traffic" aria-label="Window controls">
      <button
        class="traffic-button traffic-button--close"
        type="button"
        title="Close"
        aria-label="Close"
        onClick={() => void appWindow()?.close()}
      />
      <button
        class="traffic-button traffic-button--minimize"
        type="button"
        title="Minimize"
        aria-label="Minimize"
        onClick={() => void appWindow()?.minimize()}
      />
      <button
        class="traffic-button traffic-button--maximize"
        type="button"
        title="Maximize"
        aria-label="Maximize"
        onClick={() => void appWindow()?.toggleMaximize()}
      />
    </div>
  );
}

function WindowControls() {
  return (
    <div class="window-controls" aria-label="Window controls">
      <WindowControlButton
        label="Minimize"
        onClick={() => void appWindow()?.minimize()}
      >
        <Minus size={15} />
      </WindowControlButton>
      <WindowControlButton
        label="Maximize"
        onClick={() => void appWindow()?.toggleMaximize()}
      >
        <Square size={13} />
      </WindowControlButton>
      <WindowControlButton
        label="Close"
        danger
        onClick={() => void appWindow()?.close()}
      >
        <X size={16} />
      </WindowControlButton>
    </div>
  );
}

interface WindowControlButtonProps {
  children: JSX.Element;
  danger?: boolean;
  label: string;
  onClick: () => void;
}

function WindowControlButton(props: WindowControlButtonProps) {
  const handleClick = () => {
    void props.onClick();
  };

  return (
    <button
      classList={{
        "window-control": true,
        "window-control--danger": Boolean(props.danger),
      }}
      type="button"
      title={props.label}
      aria-label={props.label}
      onClick={handleClick}
    >
      {props.children}
    </button>
  );
}

function StatusBar(props: { onOpenSettings?: () => void }) {
  return (
    <footer class="app-statusbar">
      <div class="app-statusbar__group">
        <span class="status-profile">Local Profile</span>
        <button
          class="status-iconbutton"
          type="button"
          title="Settings"
          aria-label="Settings"
          onClick={() => props.onOpenSettings?.()}
        >
          <Settings size={14} />
        </button>
      </div>
      <div class="app-statusbar__group">
        <AgentStatusPill />
      </div>
    </footer>
  );
}
