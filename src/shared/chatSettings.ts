// Client-local chat-rendering preferences: message font/size (inherit from the
// global appearance or override), and toggles for the provider avatar, the model
// name, and collapsing tool work under one spoiler.
//
// Like appearance, these are per-client UI state. Font/size/visibility apply as
// CSS variables + `data-chat-*` attributes on the document root (consumed by
// App.css), while `collapseWork` is read reactively by the chat renderer. The
// store is a module-level Solid signal so both the settings tab and the chat
// view observe the same source of truth.

import { createSignal } from "solid-js";

import { SANS_FONT_OPTIONS, type SansFontId } from "./appearance";

/** "inherit" uses the global interface font; otherwise a specific family. */
export type ChatFontChoice = "inherit" | SansFontId;
/** "inherit" uses the default chat size; otherwise an explicit pixel size. */
export type ChatFontSizeChoice = "inherit" | number;

export interface ChatSettings {
  font: ChatFontChoice;
  fontSize: ChatFontSizeChoice;
  hideAvatar: boolean;
  hideModelName: boolean;
  collapseWork: boolean;
}

export const CHAT_FONT_SIZE = { min: 12, max: 20, step: 1, default: 14 } as const;

export const DEFAULT_CHAT_SETTINGS: ChatSettings = {
  font: "inherit",
  fontSize: "inherit",
  hideAvatar: false,
  hideModelName: false,
  collapseWork: false,
};

const STORAGE_KEY = "mothership.chat.v1";

function clampSize(value: number): number {
  if (!Number.isFinite(value)) {
    return CHAT_FONT_SIZE.default;
  }
  return Math.min(CHAT_FONT_SIZE.max, Math.max(CHAT_FONT_SIZE.min, Math.round(value)));
}

function loadChatSettings(): ChatSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return { ...DEFAULT_CHAT_SETTINGS };
    }
    const parsed = JSON.parse(raw) as Partial<ChatSettings>;
    const fontValid =
      parsed.font === "inherit" ||
      SANS_FONT_OPTIONS.some((option) => option.id === parsed.font);
    return {
      font: fontValid ? (parsed.font as ChatFontChoice) : DEFAULT_CHAT_SETTINGS.font,
      fontSize:
        parsed.fontSize === "inherit"
          ? "inherit"
          : typeof parsed.fontSize === "number"
            ? clampSize(parsed.fontSize)
            : DEFAULT_CHAT_SETTINGS.fontSize,
      hideAvatar: Boolean(parsed.hideAvatar),
      hideModelName: Boolean(parsed.hideModelName),
      collapseWork: Boolean(parsed.collapseWork),
    };
  } catch {
    return { ...DEFAULT_CHAT_SETTINGS };
  }
}

const [chatSettings, setChatSettingsSignal] =
  createSignal<ChatSettings>(loadChatSettings());

/** Reactive accessor — read inside a component to track chat-pref changes. */
export { chatSettings };

function toggleAttribute(root: HTMLElement, name: string, on: boolean) {
  if (on) {
    root.setAttribute(name, "");
  } else {
    root.removeAttribute(name);
  }
}

/** Push the preferences onto the document root (CSS vars + data attributes). */
export function applyChatSettings(settings: ChatSettings): void {
  if (typeof document === "undefined") {
    return;
  }
  const root = document.documentElement;

  if (settings.font === "inherit") {
    root.style.removeProperty("--chat-font");
  } else {
    const stack = SANS_FONT_OPTIONS.find((option) => option.id === settings.font);
    root.style.setProperty("--chat-font", stack?.stack ?? "inherit");
  }

  if (settings.fontSize === "inherit") {
    root.style.removeProperty("--chat-font-size");
  } else {
    root.style.setProperty("--chat-font-size", `${clampSize(settings.fontSize)}px`);
  }

  toggleAttribute(root, "data-chat-hide-avatar", settings.hideAvatar);
  toggleAttribute(root, "data-chat-hide-model", settings.hideModelName);
  toggleAttribute(root, "data-chat-collapse-work", settings.collapseWork);
}

/** Merge a patch, persist, apply, and publish to the signal. */
export function updateChatSettings(patch: Partial<ChatSettings>): ChatSettings {
  const next: ChatSettings = { ...chatSettings(), ...patch };
  if (typeof next.fontSize === "number") {
    next.fontSize = clampSize(next.fontSize);
  }
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Best effort — a storage failure shouldn't block the live update.
  }
  applyChatSettings(next);
  setChatSettingsSignal(next);
  return next;
}
