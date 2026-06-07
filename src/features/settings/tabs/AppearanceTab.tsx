import { For, Show } from "solid-js";
import { Check, RotateCcw } from "lucide-solid";

import {
  ACCENT_OPTIONS,
  DEFAULT_APPEARANCE,
  MONO_FONT_OPTIONS,
  PALETTE_OPTIONS,
  SANS_FONT_OPTIONS,
  SCALE,
  THEME_OPTIONS,
  appearance,
  resolvedTheme,
  updateAppearance,
  type AppearanceSettings,
} from "../../../shared/appearance";

/**
 * Appearance tab: theme mode, accent, interface + code fonts, and UI scale.
 * Pure client-side preference state (localStorage) held in the shared
 * `appearance` signal; every change is applied live via {@link updateAppearance},
 * which writes CSS variables / data attributes on the document root, so the
 * whole app re-skins instantly.
 */
export function AppearanceTab() {
  const settings = appearance;

  const update = (patch: Partial<AppearanceSettings>) => updateAppearance(patch);

  const isDefault = () => {
    const current = settings();
    return (
      current.theme === DEFAULT_APPEARANCE.theme &&
      current.palette === DEFAULT_APPEARANCE.palette &&
      current.accent === DEFAULT_APPEARANCE.accent &&
      current.sans === DEFAULT_APPEARANCE.sans &&
      current.mono === DEFAULT_APPEARANCE.mono &&
      Math.abs(current.scale - DEFAULT_APPEARANCE.scale) < 0.001
    );
  };

  return (
    <div class="settings-pane__inner">
      <div class="settings-pane__intro">
        <h2>Appearance</h2>
        <p>
          Tune the look of Mothership. These preferences are stored on this
          device and apply instantly across the whole app.
        </p>
      </div>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Mode</h3>
          <p>Light, dark, or follow the operating system.</p>
        </div>
        <div class="option-grid">
          <For each={THEME_OPTIONS}>
            {(theme) => (
              <button
                type="button"
                classList={{
                  "option-card": true,
                  "option-card--active": settings().theme === theme.id,
                }}
                onClick={() => update({ theme: theme.id })}
              >
                <span
                  class="theme-swatch"
                  aria-hidden="true"
                  style={{
                    "background-color": theme.swatch[1],
                  }}
                >
                  <span
                    class="theme-swatch__shell"
                    style={{ "background-color": theme.swatch[0] }}
                  />
                  <span
                    class="theme-swatch__deep"
                    style={{ "background-color": theme.swatch[1] }}
                  />
                </span>
                <span class="option-card__label">
                  {theme.label}
                  <Show when={settings().theme === theme.id}>
                    <span class="option-card__check">
                      <Check size={14} />
                    </span>
                  </Show>
                </span>
                <span class="option-card__hint">{theme.description}</span>
              </button>
            )}
          </For>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Palette</h3>
          <p>The surface color flavor — applies in both light and dark.</p>
        </div>
        <div class="option-grid">
          <For each={PALETTE_OPTIONS}>
            {(palette) => {
              // Show the swatch for the active mode — the palette renders light
              // in light mode, so the preview must too.
              const sw = () =>
                resolvedTheme() === "light"
                  ? palette.lightSwatch
                  : palette.darkSwatch;
              return (
                <button
                  type="button"
                  classList={{
                    "option-card": true,
                    "option-card--active": settings().palette === palette.id,
                  }}
                  onClick={() => update({ palette: palette.id })}
                >
                  <span
                    class="theme-swatch"
                    aria-hidden="true"
                    style={{ "background-color": sw()[1] }}
                  >
                    <span
                      class="theme-swatch__shell"
                      style={{ "background-color": sw()[0] }}
                    />
                    <span
                      class="theme-swatch__deep"
                      style={{ "background-color": sw()[1] }}
                    />
                  </span>
                  <span class="option-card__label">
                    {palette.label}
                    <Show when={settings().palette === palette.id}>
                      <span class="option-card__check">
                        <Check size={14} />
                      </span>
                    </Show>
                  </span>
                  <span class="option-card__hint">{palette.description}</span>
                </button>
              );
            }}
          </For>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Accent</h3>
          <p>The highlight color for buttons, selections, and focus.</p>
        </div>
        <div class="option-grid">
          <For each={ACCENT_OPTIONS}>
            {(accent) => (
              <button
                type="button"
                classList={{
                  "option-card": true,
                  "accent-card": true,
                  "option-card--active": settings().accent === accent.id,
                }}
                onClick={() => update({ accent: accent.id })}
              >
                <span
                  class="accent-dot"
                  aria-hidden="true"
                  style={{ "background-color": accent.color }}
                />
                <span class="option-card__label">
                  {accent.label}
                  <Show when={settings().accent === accent.id}>
                    <span class="option-card__check">
                      <Check size={14} />
                    </span>
                  </Show>
                </span>
              </button>
            )}
          </For>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Typography</h3>
          <p>Pick the interface and code typefaces.</p>
        </div>
        <div class="scope-grid">
          <label class="field-row">
            <span>Interface font</span>
            <select
              class="settings-select"
              value={settings().sans}
              onChange={(event) =>
                update({
                  sans: event.currentTarget
                    .value as AppearanceSettings["sans"],
                })
              }
            >
              <For each={SANS_FONT_OPTIONS}>
                {(font) => <option value={font.id}>{font.label}</option>}
              </For>
            </select>
          </label>
          <label class="field-row">
            <span>Code font</span>
            <select
              class="settings-select"
              value={settings().mono}
              onChange={(event) =>
                update({
                  mono: event.currentTarget
                    .value as AppearanceSettings["mono"],
                })
              }
            >
              <For each={MONO_FONT_OPTIONS}>
                {(font) => <option value={font.id}>{font.label}</option>}
              </For>
            </select>
          </label>
        </div>
        <p
          class="muted-line"
          style={{
            "font-family": "var(--app-font-mono)",
            "font-size": "12.5px",
          }}
        >
          {"const sum = (a, b) => a + b; // 0123456789 {}[]()<>"}
        </p>
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>UI scale</h3>
          <p>
            Zoom the interface. The window controls and status bar stay at their
            native size.
          </p>
        </div>
        <div class="scale-control">
          <div class="scale-control__row">
            <input
              type="range"
              min={SCALE.min}
              max={SCALE.max}
              step={SCALE.step}
              value={settings().scale}
              onInput={(event) =>
                update({ scale: Number(event.currentTarget.value) })
              }
            />
            <span class="scale-value">
              {Math.round(settings().scale * 100)}%
            </span>
          </div>
        </div>
      </section>

      <div class="prompt-actions">
        <span class="muted-line">
          {isDefault() ? "Using the default appearance." : "Custom appearance."}
        </span>
        <button
          class="settings-secondary-button"
          type="button"
          disabled={isDefault()}
          onClick={() => update({ ...DEFAULT_APPEARANCE })}
        >
          <RotateCcw size={15} />
          Reset to defaults
        </button>
      </div>
    </div>
  );
}
