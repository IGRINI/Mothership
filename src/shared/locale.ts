import { createSignal } from "solid-js";

export type UiLocale = "en" | "ru";

export const UI_LOCALE_OPTIONS: { id: UiLocale; label: string }[] = [
  { id: "en", label: "English" },
  { id: "ru", label: "Русский" },
];

const STORAGE_KEY = "mothership.ui-locale.v1";
const DEFAULT_LOCALE: UiLocale = "en";

function isUiLocale(value: unknown): value is UiLocale {
  return value === "en" || value === "ru";
}

function loadUiLocale(): UiLocale {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return isUiLocale(stored) ? stored : DEFAULT_LOCALE;
  } catch {
    return DEFAULT_LOCALE;
  }
}

const [uiLocale, setUiLocaleSignal] = createSignal<UiLocale>(loadUiLocale());

export { uiLocale };

export function setUiLocale(locale: UiLocale): UiLocale {
  try {
    localStorage.setItem(STORAGE_KEY, locale);
  } catch {
    // Best effort: localization must still update for the current session.
  }
  setUiLocaleSignal(locale);
  document.documentElement.lang = locale;
  return locale;
}

if (typeof document !== "undefined") {
  document.documentElement.lang = uiLocale();
}
