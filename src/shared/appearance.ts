// Client-local appearance preferences: theme mode, accent, fonts, and UI scale.
//
// These are per-client UI state (like the active project) — never backend or
// global. They persist to localStorage and apply as CSS custom properties +
// data attributes on the document root, so the whole app re-skins instantly
// with no round-trip.
//
// Theme model: System / Light / Dark. The stored choice is resolved to a
// concrete "light" | "dark" written to `data-theme` (which App.css keys on);
// "system" tracks the OS `prefers-color-scheme` live. App.css declares the dark
// palette on :root and the full light palette under `:root[data-theme=light]`.

import { createSignal } from "solid-js";

export type ThemeId = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";
export type PaletteId = "aurora" | "midnight" | "graphite" | "nebula";
export type AccentId = "azure" | "iris" | "emerald" | "amber" | "rose" | "cyan";
export type SansFontId = "system" | "inter" | "segoe" | "verdana" | "georgia";
export type MonoFontId =
  | "system"
  | "cascadia"
  | "consolas"
  | "jetbrains"
  | "courier";

export interface AppearanceSettings {
  /** Light/dark mode (System tracks the OS). */
  theme: ThemeId;
  /** Surface color flavor, applied within either mode. */
  palette: PaletteId;
  accent: AccentId;
  sans: SansFontId;
  mono: MonoFontId;
  /** UI zoom multiplier applied to the app body (chrome stays at 100%). */
  scale: number;
}

export interface ThemeOption {
  id: ThemeId;
  label: string;
  description: string;
  /** [shell background, deep background] preview swatch. */
  swatch: [string, string];
}

export interface PaletteOption {
  id: PaletteId;
  label: string;
  description: string;
  /** [shell, deep] preview swatch in dark mode. */
  darkSwatch: [string, string];
  /** [shell, deep] preview swatch in light mode (matches the real render). */
  lightSwatch: [string, string];
}

export interface AccentOption {
  id: AccentId;
  label: string;
  color: string;
}

export interface FontOption<Id extends string> {
  id: Id;
  label: string;
  stack: string;
}

export const THEME_OPTIONS: ThemeOption[] = [
  {
    id: "system",
    label: "System",
    description: "Follow the operating system's light or dark setting.",
    swatch: ["#f6f8fb", "#070f18"],
  },
  {
    id: "light",
    label: "Light",
    description: "A bright, low-contrast surface for well-lit rooms.",
    swatch: ["#f6f8fb", "#e8edf3"],
  },
  {
    id: "dark",
    label: "Dark",
    description: "A deep, low-glare surface for dim rooms.",
    swatch: ["#07101a", "#050a10"],
  },
];

export const PALETTE_OPTIONS: PaletteOption[] = [
  {
    id: "aurora",
    label: "Aurora",
    description: "The signature deep-navy Mothership flavor.",
    darkSwatch: ["#07101a", "#050a10"],
    lightSwatch: ["#f6f8fb", "#e8edf3"],
  },
  {
    id: "midnight",
    label: "Midnight",
    description: "Cooler and darker — closer to true black.",
    darkSwatch: ["#080b13", "#050609"],
    lightSwatch: ["#f7f8fa", "#eaecf1"],
  },
  {
    id: "graphite",
    label: "Graphite",
    description: "Neutral, desaturated slate for a calmer surface.",
    darkSwatch: ["#15171a", "#0c0d0e"],
    lightSwatch: ["#f8f8f9", "#ededee"],
  },
  {
    id: "nebula",
    label: "Nebula",
    description: "A subtle violet tint over the base.",
    darkSwatch: ["#160d24", "#0d061b"],
    lightSwatch: ["#f8f6fb", "#ece8f3"],
  },
];

export const ACCENT_OPTIONS: AccentOption[] = [
  { id: "azure", label: "Azure", color: "#216ce3" },
  { id: "iris", label: "Iris", color: "#7c5cff" },
  { id: "emerald", label: "Emerald", color: "#18a558" },
  { id: "amber", label: "Amber", color: "#e0a32e" },
  { id: "rose", label: "Rose", color: "#e25577" },
  { id: "cyan", label: "Cyan", color: "#1fa8c9" },
];

export const SANS_FONT_OPTIONS: FontOption<SansFontId>[] = [
  {
    id: "system",
    label: "System",
    stack: `ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif`,
  },
  {
    id: "inter",
    label: "Inter",
    stack: `Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif`,
  },
  {
    id: "segoe",
    label: "Segoe UI",
    stack: `"Segoe UI", ui-sans-serif, system-ui, sans-serif`,
  },
  {
    id: "verdana",
    label: "Verdana",
    stack: `Verdana, Geneva, ui-sans-serif, sans-serif`,
  },
  {
    id: "georgia",
    label: "Georgia",
    stack: `Georgia, Cambria, "Times New Roman", ui-serif, serif`,
  },
];

export const MONO_FONT_OPTIONS: FontOption<MonoFontId>[] = [
  {
    id: "system",
    label: "System",
    stack: `ui-monospace, "Cascadia Code", Consolas, "Courier New", monospace`,
  },
  {
    id: "cascadia",
    label: "Cascadia Code",
    stack: `"Cascadia Code", "Cascadia Mono", ui-monospace, Consolas, monospace`,
  },
  {
    id: "consolas",
    label: "Consolas",
    stack: `Consolas, "Cascadia Mono", ui-monospace, monospace`,
  },
  {
    id: "jetbrains",
    label: "JetBrains Mono",
    stack: `"JetBrains Mono", ui-monospace, "Cascadia Code", Consolas, monospace`,
  },
  {
    id: "courier",
    label: "Courier",
    stack: `"Courier New", ui-monospace, monospace`,
  },
];

export const SCALE = { min: 0.85, max: 1.3, step: 0.05, default: 1 } as const;

const STORAGE_KEY = "mothership.appearance.v1";

export const DEFAULT_APPEARANCE: AppearanceSettings = {
  theme: "system",
  palette: "aurora",
  accent: "azure",
  sans: "system",
  mono: "system",
  scale: 1,
};

// Before palettes were a separate axis, the four dark flavors lived in the
// `theme` field. A legacy value there now means: dark mode + that palette.
const LEGACY_PALETTE_THEMES = new Set<PaletteId>([
  "aurora",
  "midnight",
  "graphite",
  "nebula",
]);

function clampScale(value: number): number {
  if (!Number.isFinite(value)) {
    return SCALE.default;
  }
  return Math.min(SCALE.max, Math.max(SCALE.min, Math.round(value * 100) / 100));
}

function isOneOf<T extends string>(value: unknown, options: readonly T[]): value is T {
  return typeof value === "string" && (options as readonly string[]).includes(value);
}

// Resolve the stored {theme, palette} pair, migrating the legacy layout where a
// dark flavor lived in the `theme` field (→ dark mode + that palette).
function coerceThemeAndPalette(parsed: Partial<AppearanceSettings>): {
  theme: ThemeId;
  palette: PaletteId;
} {
  const rawTheme = parsed.theme;
  if (typeof rawTheme === "string" && LEGACY_PALETTE_THEMES.has(rawTheme as PaletteId)) {
    return { theme: "dark", palette: rawTheme as PaletteId };
  }
  return {
    theme: isOneOf(rawTheme, THEME_OPTIONS.map((option) => option.id))
      ? rawTheme
      : DEFAULT_APPEARANCE.theme,
    palette: isOneOf(parsed.palette, PALETTE_OPTIONS.map((option) => option.id))
      ? parsed.palette
      : DEFAULT_APPEARANCE.palette,
  };
}

/** Read the stored preferences, falling back to defaults for any missing or
 * unrecognized field (so an older/partial blob never breaks the app). */
export function loadAppearance(): AppearanceSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return { ...DEFAULT_APPEARANCE };
    }
    const parsed = JSON.parse(raw) as Partial<AppearanceSettings>;
    const { theme, palette } = coerceThemeAndPalette(parsed);
    return {
      theme,
      palette,
      accent: isOneOf(parsed.accent, ACCENT_OPTIONS.map((option) => option.id))
        ? parsed.accent
        : DEFAULT_APPEARANCE.accent,
      sans: isOneOf(parsed.sans, SANS_FONT_OPTIONS.map((option) => option.id))
        ? parsed.sans
        : DEFAULT_APPEARANCE.sans,
      mono: isOneOf(parsed.mono, MONO_FONT_OPTIONS.map((option) => option.id))
        ? parsed.mono
        : DEFAULT_APPEARANCE.mono,
      scale: clampScale(parsed.scale ?? DEFAULT_APPEARANCE.scale),
    };
  } catch {
    return { ...DEFAULT_APPEARANCE };
  }
}

function prefersDark(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
  );
}

/** Resolve the stored choice to a concrete palette ("system" tracks the OS). */
export function resolveTheme(theme: ThemeId): ResolvedTheme {
  if (theme === "system") {
    return prefersDark() ? "dark" : "light";
  }
  return theme;
}

/** Apply the preferences to the live document (data attributes + CSS vars). */
export function applyAppearance(settings: AppearanceSettings): void {
  if (typeof document === "undefined") {
    return;
  }
  const root = document.documentElement;
  const resolved = resolveTheme(settings.theme);
  // `data-theme` is the concrete palette CSS keys on; `data-theme-mode` is the
  // user's choice (so the settings UI can highlight "System" even when it
  // currently resolves to dark).
  root.dataset.theme = resolved;
  root.dataset.themeMode = settings.theme;
  root.dataset.palette = settings.palette;
  root.dataset.accent = settings.accent;
  // Let native widgets (scrollbars, form controls, autofill) match the palette.
  root.style.colorScheme = resolved;

  const sans =
    SANS_FONT_OPTIONS.find((option) => option.id === settings.sans) ??
    SANS_FONT_OPTIONS[0];
  const mono =
    MONO_FONT_OPTIONS.find((option) => option.id === settings.mono) ??
    MONO_FONT_OPTIONS[0];
  root.style.setProperty("--app-font-sans", sans.stack);
  root.style.setProperty("--app-font-mono", mono.stack);
  root.style.setProperty("--app-ui-scale", String(clampScale(settings.scale)));
}

// Module-level source of truth so the settings tab AND the status bar (and any
// other consumer) observe the same appearance. Initialized from storage.
const [appearance, setAppearanceSignal] =
  createSignal<AppearanceSettings>(loadAppearance());

/** Reactive accessor — read inside a component to track appearance changes. */
export { appearance };

/** The live resolved palette ("light" | "dark"), reactive to choice + OS. */
const [resolvedTheme, setResolvedTheme] = createSignal<ResolvedTheme>(
  resolveTheme(appearance().theme),
);
export { resolvedTheme };

/** Persist and apply in one step (the settings UI calls this on every change). */
export function saveAppearance(settings: AppearanceSettings): AppearanceSettings {
  const next: AppearanceSettings = { ...settings, scale: clampScale(settings.scale) };
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Best effort: a storage failure shouldn't block re-skinning the live UI.
  }
  applyAppearance(next);
  setAppearanceSignal(next);
  setResolvedTheme(resolveTheme(next.theme));
  return next;
}

/** Merge a patch into the current appearance, then persist + apply. */
export function updateAppearance(patch: Partial<AppearanceSettings>): AppearanceSettings {
  return saveAppearance({ ...appearance(), ...patch });
}

// When the OS theme flips and the user is on "System", re-resolve live.
if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const onChange = () => {
    if (appearance().theme === "system") {
      applyAppearance(appearance());
      setResolvedTheme(resolveTheme(appearance().theme));
    }
  };
  if (typeof media.addEventListener === "function") {
    media.addEventListener("change", onChange);
  } else if (typeof media.addListener === "function") {
    // Safari < 14 / older WebViews.
    media.addListener(onChange);
  }
}
