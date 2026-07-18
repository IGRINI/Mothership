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
import {
  setUiLocale,
  uiLocale,
  UI_LOCALE_OPTIONS,
  type UiLocale,
} from "../../../shared/locale";
import { settingsCopy } from "../settings-copy";
import {
  sectionMatches,
  SettingsHighlight,
  type SettingsSearchState,
} from "../settings-search";

/**
 * Appearance tab: theme mode, accent, interface + code fonts, and UI scale.
 * Pure client-side preference state (localStorage) held in the shared
 * `appearance` signal; every change is applied live via {@link updateAppearance},
 * which writes CSS variables / data attributes on the document root, so the
 * whole app re-skins instantly.
 */
export function AppearanceTab(props: { search: SettingsSearchState }) {
  const settings = appearance;
  const copy = () => settingsCopy().appearance;

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
        <h2>
          <SettingsHighlight text={copy().title} search={props.search} />
        </h2>
        <p>
          <SettingsHighlight text={copy().intro} search={props.search} />
        </p>
      </div>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.language",
          ),
        }}
        data-settings-section="appearance.language"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().interfaceLanguageTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().interfaceLanguageDescription}
              search={props.search}
            />
          </p>
        </div>
        <label class="field-row">
          <span>{copy().interfaceLanguageTitle}</span>
          <select
            class="settings-select"
            value={uiLocale()}
            onChange={(event) =>
              setUiLocale(event.currentTarget.value as UiLocale)
            }
          >
            <For each={UI_LOCALE_OPTIONS}>
              {(locale) => <option value={locale.id}>{locale.label}</option>}
            </For>
          </select>
        </label>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.mode",
          ),
        }}
        data-settings-section="appearance.mode"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().modeTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().modeDescription}
              search={props.search}
            />
          </p>
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
                  <SettingsHighlight
                    text={copy().themeOptions[theme.id]?.label ?? theme.label}
                    search={props.search}
                  />
                  <Show when={settings().theme === theme.id}>
                    <span class="option-card__check">
                      <Check size={14} />
                    </span>
                  </Show>
                </span>
                <span class="option-card__hint">
                  <SettingsHighlight
                    text={
                      copy().themeOptions[theme.id]?.description ??
                      theme.description
                    }
                    search={props.search}
                  />
                </span>
              </button>
            )}
          </For>
        </div>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.palette",
          ),
        }}
        data-settings-section="appearance.palette"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().paletteTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().paletteDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="option-grid">
          <For each={PALETTE_OPTIONS}>
            {(palette) => {
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
                    <SettingsHighlight
                      text={
                        copy().paletteOptions[palette.id]?.label ??
                        palette.label
                      }
                      search={props.search}
                    />
                    <Show when={settings().palette === palette.id}>
                      <span class="option-card__check">
                        <Check size={14} />
                      </span>
                    </Show>
                  </span>
                  <span class="option-card__hint">
                    <SettingsHighlight
                      text={
                        copy().paletteOptions[palette.id]?.description ??
                        palette.description
                      }
                      search={props.search}
                    />
                  </span>
                </button>
              );
            }}
          </For>
        </div>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.accent",
          ),
        }}
        data-settings-section="appearance.accent"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().accentTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().accentDescription}
              search={props.search}
            />
          </p>
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
                  <SettingsHighlight
                    text={copy().accentOptions[accent.id] ?? accent.label}
                    search={props.search}
                  />
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

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.typography",
          ),
        }}
        data-settings-section="appearance.typography"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().typographyTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().typographyDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="scope-grid">
          <label class="field-row">
            <span>{copy().interfaceFont}</span>
            <select
              class="settings-select"
              value={settings().sans}
              onChange={(event) =>
                update({
                  sans: event.currentTarget.value as AppearanceSettings["sans"],
                })
              }
            >
              <For each={SANS_FONT_OPTIONS}>
                {(font) => (
                  <option value={font.id}>
                    {copy().fontOptions[font.id] ?? font.label}
                  </option>
                )}
              </For>
            </select>
          </label>
          <label class="field-row">
            <span>{copy().codeFont}</span>
            <select
              class="settings-select"
              value={settings().mono}
              onChange={(event) =>
                update({
                  mono: event.currentTarget.value as AppearanceSettings["mono"],
                })
              }
            >
              <For each={MONO_FONT_OPTIONS}>
                {(font) => (
                  <option value={font.id}>
                    {copy().fontOptions[font.id] ?? font.label}
                  </option>
                )}
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

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "appearance.scale",
          ),
        }}
        data-settings-section="appearance.scale"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().scaleTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().scaleDescription}
              search={props.search}
            />
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
          {isDefault() ? copy().defaultState : copy().customState}
        </span>
        <button
          class="settings-secondary-button"
          type="button"
          disabled={isDefault()}
          onClick={() => update({ ...DEFAULT_APPEARANCE })}
        >
          <RotateCcw size={15} />
          {copy().reset}
        </button>
      </div>
    </div>
  );
}
