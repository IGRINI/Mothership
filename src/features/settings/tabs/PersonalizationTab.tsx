import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { Check, Globe, Save, Trash2 } from "lucide-solid";

import type {
  ConnectorProviderSummary,
  PersonalizationSettings,
} from "../../../shared/api/mothership";
import {
  getPersonalization,
  setPersonalization,
  setResponseLanguage,
} from "../../../shared/api/mothership";
import { settingsCopy } from "../settings-copy";
import {
  sectionMatches,
  SettingsHighlight,
  type SettingsSearchState,
} from "../settings-search";

type ScopeKind = "global" | "provider" | "model";

const EMPTY: PersonalizationSettings = {
  global: "",
  providers: [],
  models: [],
  responseLanguage: { languageId: "auto", customLanguage: "" },
};

const RESPONSE_LANGUAGE_IDS = [
  "auto",
  "en",
  "ru",
  "es",
  "de",
  "fr",
  "it",
  "pt",
  "zh",
  "ja",
  "ko",
  "uk",
  "pl",
  "tr",
  "ar",
  "hi",
  "custom",
] as const;

export function PersonalizationTab(props: {
  providers: ConnectorProviderSummary[];
  onError: (message: string) => void;
  onStatus: (message: string) => void;
  search: SettingsSearchState;
}) {
  const [settings, setSettings] = createSignal<PersonalizationSettings>(EMPTY);
  const [scope, setScope] = createSignal<ScopeKind>("global");
  const [providerId, setProviderId] = createSignal<string>("");
  const [modelId, setModelId] = createSignal<string>("");
  const [draft, setDraft] = createSignal("");
  const [languageId, setLanguageId] = createSignal("auto");
  const [customLanguage, setCustomLanguage] = createSignal("");
  const [savingInstructions, setSavingInstructions] = createSignal(false);
  const [savingLanguage, setSavingLanguage] = createSignal(false);
  const copy = () => settingsCopy().personalization;

  const providerOptions = () => props.providers;
  const modelOptions = () =>
    props.providers.find((provider) => provider.id === providerId())?.models ??
    [];

  const storedContent = createMemo(() => {
    const current = settings();
    if (scope() === "global") {
      return current.global;
    }
    if (scope() === "provider") {
      return (
        current.providers.find((item) => item.providerId === providerId())
          ?.content ?? ""
      );
    }
    return (
      current.models.find(
        (item) =>
          item.providerId === providerId() && item.modelId === modelId(),
      )?.content ?? ""
    );
  });

  const dirty = () => draft() !== storedContent();
  const languageDirty = () =>
    languageId() !== settings().responseLanguage.languageId ||
    customLanguage() !== settings().responseLanguage.customLanguage;
  const resetDraft = () => setDraft(storedContent());
  const syncLanguage = (loaded: PersonalizationSettings) => {
    setLanguageId(loaded.responseLanguage.languageId);
    setCustomLanguage(loaded.responseLanguage.customLanguage);
  };

  onMount(async () => {
    try {
      const loaded = await getPersonalization();
      setSettings(loaded);
      syncLanguage(loaded);
      const firstProvider = props.providers[0];
      if (firstProvider) {
        setProviderId(firstProvider.id);
        setModelId(firstProvider.models[0]?.id ?? "");
      }
      resetDraft();
    } catch (error) {
      props.onError(errorMessage(error));
    }
  });

  function chooseScope(next: ScopeKind) {
    setScope(next);
    if (next !== "global" && !providerId()) {
      const firstProvider = props.providers[0];
      if (firstProvider) {
        setProviderId(firstProvider.id);
        setModelId(firstProvider.models[0]?.id ?? "");
      }
    }
    if (next === "model" && !modelId()) {
      setModelId(modelOptions()[0]?.id ?? "");
    }
    resetDraft();
  }

  function chooseProvider(id: string) {
    setProviderId(id);
    setModelId(modelOptions()[0]?.id ?? "");
    resetDraft();
  }

  function chooseModel(id: string) {
    setModelId(id);
    resetDraft();
  }

  async function persist(content: string) {
    const scopeProvider = scope() === "global" ? null : providerId();
    const scopeModel = scope() === "model" ? modelId() : null;
    setSavingInstructions(true);
    try {
      const updated = await setPersonalization(scopeProvider, scopeModel, content);
      setSettings(updated);
      syncLanguage(updated);
      setDraft(content.trim());
      props.onStatus(content.trim() ? copy().saved : copy().cleared);
    } catch (error) {
      props.onError(errorMessage(error));
    } finally {
      setSavingInstructions(false);
    }
  }

  async function persistResponseLanguage() {
    setSavingLanguage(true);
    try {
      const updated = await setResponseLanguage(languageId(), customLanguage());
      setSettings(updated);
      syncLanguage(updated);
      props.onStatus(copy().responseLanguageSaved);
    } catch (error) {
      props.onError(errorMessage(error));
    } finally {
      setSavingLanguage(false);
    }
  }

  const canEditScopedScope = () =>
    scope() === "global" ||
    (scope() === "provider" && Boolean(providerId())) ||
    (scope() === "model" && Boolean(providerId()) && Boolean(modelId()));

  const scopeLabel = () => {
    if (scope() === "global") {
      return copy().labels.globalApplied;
    }
    const provider = props.providers.find((item) => item.id === providerId());
    if (scope() === "provider") {
      return copy().labels.provider(provider?.label ?? providerId());
    }
    const model = modelOptions().find((item) => item.id === modelId());
    return copy().labels.model(
      provider?.label ?? providerId(),
      model?.label ?? modelId(),
    );
  };

  const languageCanSave = () =>
    languageDirty() &&
    !savingLanguage() &&
    (languageId() !== "custom" || customLanguage().trim().length > 0);

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
            "personalization.response-language",
          ),
        }}
        data-settings-section="personalization.response-language"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().responseLanguageTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().responseLanguageDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="scope-grid">
          <label class="field-row">
            <span>{copy().responseLanguageSelect}</span>
            <select
              class="settings-select"
              value={languageId()}
              onChange={(event) => setLanguageId(event.currentTarget.value)}
            >
              <For each={RESPONSE_LANGUAGE_IDS}>
                {(id) => (
                  <option value={id}>{copy().responseLanguages[id]}</option>
                )}
              </For>
            </select>
          </label>
          <Show when={languageId() === "custom"}>
            <label class="field-row">
              <span>{copy().customLanguage}</span>
              <input
                class="settings-input"
                type="text"
                value={customLanguage()}
                placeholder={copy().customLanguagePlaceholder}
                onInput={(event) => setCustomLanguage(event.currentTarget.value)}
              />
            </label>
          </Show>
        </div>
        <div class="prompt-actions">
          <span class="muted-line">{copy().responseLanguageChatNote}</span>
          <button
            class="settings-primary-button"
            type="button"
            disabled={!languageCanSave()}
            onClick={() => void persistResponseLanguage()}
          >
            <Save size={15} />
            {savingLanguage() ? settingsCopy().common.saving : settingsCopy().common.save}
          </button>
        </div>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "personalization.scope",
          ),
        }}
        data-settings-section="personalization.scope"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().scopeTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().scopeDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="segmented segmented--block" role="group">
          <button
            type="button"
            classList={{
              segmented__option: true,
              "segmented__option--active": scope() === "global",
            }}
            onClick={() => chooseScope("global")}
          >
            <Globe size={14} />
            {copy().global}
          </button>
          <button
            type="button"
            classList={{
              segmented__option: true,
              "segmented__option--active": scope() === "provider",
            }}
            onClick={() => chooseScope("provider")}
          >
            {copy().perProvider}
          </button>
          <button
            type="button"
            classList={{
              segmented__option: true,
              "segmented__option--active": scope() === "model",
            }}
            onClick={() => chooseScope("model")}
          >
            {copy().perModel}
          </button>
        </div>

        <Show when={scope() !== "global"}>
          <Show
            when={providerOptions().length > 0}
            fallback={<p class="muted-line">{copy().noConnectors}</p>}
          >
            <div class="scope-grid">
              <label class="field-row">
                <span>{copy().provider}</span>
                <select
                  class="settings-select"
                  value={providerId()}
                  onChange={(event) => chooseProvider(event.currentTarget.value)}
                >
                  <For each={providerOptions()}>
                    {(provider) => (
                      <option value={provider.id}>{provider.label}</option>
                    )}
                  </For>
                </select>
              </label>
              <Show when={scope() === "model"}>
                <label class="field-row">
                  <span>{copy().model}</span>
                  <Show
                    when={modelOptions().length > 0}
                    fallback={<p class="muted-line">{copy().noModels}</p>}
                  >
                    <select
                      class="settings-select"
                      value={modelId()}
                      onChange={(event) => chooseModel(event.currentTarget.value)}
                    >
                      <For each={modelOptions()}>
                        {(model) => (
                          <option value={model.id}>{model.label}</option>
                        )}
                      </For>
                    </select>
                  </Show>
                </label>
              </Show>
            </div>
          </Show>
        </Show>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "personalization.instructions",
          ),
        }}
        data-settings-section="personalization.instructions"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().customInstructionsTitle}
              search={props.search}
            />
          </h3>
          <p>{scopeLabel()}</p>
        </div>
        <Show
          when={canEditScopedScope()}
          fallback={<p class="muted-line">{copy().pickScope}</p>}
        >
          <textarea
            class="prompt-textarea"
            value={draft()}
            placeholder={copy().placeholder}
            spellcheck={false}
            onInput={(event) => setDraft(event.currentTarget.value)}
          />
          <div class="prompt-actions">
            <span class="char-count">{copy().characters(draft().length)}</span>
            <div class="prompt-actions__right">
              <button
                class="settings-secondary-button"
                type="button"
                style={{
                  visibility:
                    storedContent().length > 0 ? "visible" : "hidden",
                }}
                disabled={savingInstructions() || storedContent().length === 0}
                onClick={() => void persist("")}
              >
                <Trash2 size={15} />
                {copy().clear}
              </button>
              <button
                class="settings-primary-button"
                type="button"
                disabled={!dirty() || savingInstructions()}
                onClick={() => void persist(draft())}
              >
                <Save size={15} />
                {savingInstructions()
                  ? settingsCopy().common.saving
                  : settingsCopy().common.save}
              </button>
            </div>
          </div>
        </Show>
      </section>

      <SavedScopes
        settings={settings()}
        providers={props.providers}
        search={props.search}
        onOpenProvider={(id) => {
          setScope("provider");
          setProviderId(id);
          resetDraft();
        }}
        onOpenModel={(provider, model) => {
          setScope("model");
          setProviderId(provider);
          setModelId(model);
          resetDraft();
        }}
      />
    </div>
  );
}

function SavedScopes(props: {
  settings: PersonalizationSettings;
  providers: ConnectorProviderSummary[];
  search: SettingsSearchState;
  onOpenProvider: (providerId: string) => void;
  onOpenModel: (providerId: string, modelId: string) => void;
}) {
  const copy = () => settingsCopy().personalization;
  const hasAny = () =>
    props.settings.global.trim().length > 0 ||
    props.settings.providers.length > 0 ||
    props.settings.models.length > 0;

  const providerLabel = (id: string) =>
    props.providers.find((provider) => provider.id === id)?.label ?? id;

  return (
    <Show when={hasAny()}>
      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "personalization.saved",
          ),
        }}
        data-settings-section="personalization.saved"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().configuredTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().configuredDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="rule-list">
          <Show when={props.settings.global.trim().length > 0}>
            <span class="rule-chip rule-chip--allow">
              <Check size={12} />
              {copy().labels.globalScope}
            </span>
          </Show>
          <For each={props.settings.providers}>
            {(item) => (
              <button
                class="rule-chip"
                type="button"
                onClick={() => props.onOpenProvider(item.providerId)}
              >
                {providerLabel(item.providerId)}
              </button>
            )}
          </For>
          <For each={props.settings.models}>
            {(item) => (
              <button
                class="rule-chip"
                type="button"
                onClick={() => props.onOpenModel(item.providerId, item.modelId)}
              >
                {providerLabel(item.providerId)} · {item.modelId}
              </button>
            )}
          </For>
        </div>
      </section>
    </Show>
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
