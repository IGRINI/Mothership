// Appearance popover for a project's sidebar tile: an accent-color row, a set
// of lucide glyphs, a curated emoji grid, and a free-form emoji field. Fixed
// positioning (anchored to the clicked tile) so the VirtualList's overflow
// can't clip it. Selections apply immediately through the callback; "Reset"
// clears back to the default folder + theme color.

import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import {
  Bot,
  Flame,
  Folder,
  Gem,
  Heart,
  Leaf,
  Rocket,
  Star,
  Terminal,
  Zap,
  type LucideProps,
} from "lucide-solid";
import type { Component } from "solid-js";

/** The lucide glyphs offered by the picker, by stable id (`lucide:<id>`). */
export const PROJECT_LUCIDE_ICONS: Array<{
  id: string;
  Icon: Component<LucideProps>;
}> = [
  { id: "folder", Icon: Folder },
  { id: "rocket", Icon: Rocket },
  { id: "star", Icon: Star },
  { id: "zap", Icon: Zap },
  { id: "bot", Icon: Bot },
  { id: "terminal", Icon: Terminal },
  { id: "flame", Icon: Flame },
  { id: "heart", Icon: Heart },
  { id: "leaf", Icon: Leaf },
  { id: "gem", Icon: Gem },
];

const PROJECT_COLORS = [
  "#58a6ff",
  "#41d981",
  "#f0b748",
  "#e5484d",
  "#8d4dff",
  "#ff7eb6",
  "#3ddbd9",
  "#8da0b6",
];

const PROJECT_EMOJIS = [
  "🚀",
  "🔥",
  "⭐",
  "💡",
  "🤖",
  "🧪",
  "📦",
  "🛠️",
  "🎯",
  "🧠",
  "💎",
  "🌙",
  "⚡",
  "🐳",
  "🌿",
  "🎨",
];

export interface ProjectAppearanceTarget {
  projectId: string;
  icon?: string | null;
  iconColor?: string | null;
  /** Viewport anchor (the clicked tile's rect). */
  anchorX: number;
  anchorY: number;
}

export function ProjectIconPicker(props: {
  target: ProjectAppearanceTarget | null;
  onApply: (
    projectId: string,
    icon: string | null,
    iconColor: string | null,
  ) => void;
  onClose: () => void;
}) {
  let panelRef: HTMLDivElement | undefined;
  const [customEmoji, setCustomEmoji] = createSignal("");

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!props.target || !panelRef) {
        return;
      }
      if (event.target instanceof Node && !panelRef.contains(event.target)) {
        props.onClose();
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && props.target) {
        props.onClose();
      }
    };
    // Capture phase so the opening click's own bubble doesn't insta-close it.
    document.addEventListener("pointerdown", handlePointerDown, true);
    document.addEventListener("keydown", handleKeyDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", handlePointerDown, true);
      document.removeEventListener("keydown", handleKeyDown);
    });
  });

  // Keep the panel on-screen (it opens next to the tile, lists can be near the
  // window edge).
  createEffect(() => {
    const target = props.target;
    if (!target) {
      setCustomEmoji("");
      return;
    }
    requestAnimationFrame(() => {
      if (!panelRef) {
        return;
      }
      const rect = panelRef.getBoundingClientRect();
      let x = target.anchorX;
      let y = target.anchorY;
      if (rect.width + x > window.innerWidth - 8) {
        x = Math.max(8, window.innerWidth - rect.width - 8);
      }
      if (rect.height + y > window.innerHeight - 8) {
        y = Math.max(8, window.innerHeight - rect.height - 8);
      }
      panelRef.style.left = `${x}px`;
      panelRef.style.top = `${y}px`;
    });
  });

  const apply = (icon: string | null | undefined, color: string | null | undefined) => {
    const target = props.target;
    if (!target) {
      return;
    }
    props.onApply(
      target.projectId,
      icon === undefined ? target.icon ?? null : icon,
      color === undefined ? target.iconColor ?? null : color,
    );
  };

  const submitCustomEmoji = () => {
    const value = customEmoji().trim();
    if (!value) {
      return;
    }
    // First grapheme-ish: avoid storing whole sentences; combined emojis are
    // multi-codepoint, so keep up to 8 code units.
    apply(`emoji:${value.slice(0, 8)}`, undefined);
    setCustomEmoji("");
  };

  return (
    <Show when={props.target}>
      {(target) => (
        <Portal>
          <div
            ref={panelRef}
            class="icon-picker"
            role="dialog"
            aria-label="Project appearance"
            style={{
              left: `${target().anchorX}px`,
              top: `${target().anchorY}px`,
            }}
          >
            <div class="icon-picker__heading">Color</div>
            <div class="icon-picker__row">
              <For each={PROJECT_COLORS}>
                {(color) => (
                  <button
                    classList={{
                      "icon-picker__swatch": true,
                      "icon-picker__swatch--selected":
                        target().iconColor === color,
                    }}
                    type="button"
                    title={color}
                    aria-label={`Accent ${color}`}
                    style={{ background: color }}
                    onClick={() => apply(undefined, color)}
                  />
                )}
              </For>
            </div>

            <div class="icon-picker__heading">Icon</div>
            <div class="icon-picker__row">
              <For each={PROJECT_LUCIDE_ICONS}>
                {(entry) => (
                  <button
                    classList={{
                      "icon-picker__cell": true,
                      "icon-picker__cell--selected":
                        target().icon === `lucide:${entry.id}`,
                    }}
                    type="button"
                    title={entry.id}
                    onClick={() => apply(`lucide:${entry.id}`, undefined)}
                  >
                    <entry.Icon size={16} />
                  </button>
                )}
              </For>
            </div>

            <div class="icon-picker__heading">Emoji</div>
            <div class="icon-picker__row">
              <For each={PROJECT_EMOJIS}>
                {(emoji) => (
                  <button
                    classList={{
                      "icon-picker__cell": true,
                      "icon-picker__cell--selected":
                        target().icon === `emoji:${emoji}`,
                    }}
                    type="button"
                    onClick={() => apply(`emoji:${emoji}`, undefined)}
                  >
                    {emoji}
                  </button>
                )}
              </For>
            </div>

            <div class="icon-picker__custom">
              <input
                type="text"
                placeholder="Any emoji…"
                value={customEmoji()}
                maxLength={8}
                onInput={(event) => setCustomEmoji(event.currentTarget.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    submitCustomEmoji();
                  }
                }}
              />
              <button
                type="button"
                disabled={!customEmoji().trim()}
                onClick={submitCustomEmoji}
              >
                Set
              </button>
            </div>

            <button
              class="icon-picker__reset"
              type="button"
              onClick={() => apply(null, null)}
            >
              Reset to default
            </button>
          </div>
        </Portal>
      )}
    </Show>
  );
}
