import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { Check, Globe, Save, Trash2 } from "lucide-solid";

import type {
  ConnectorProviderSummary,
  PersonalizationSettings,
} from "../../../shared/api/mothership";
import {
  getPersonalization,
  setPersonalization,
} from "../../../shared/api/mothership";

type ScopeKind = "global" | "provider" | "model";

const EMPTY: PersonalizationSettings = { global: "", providers: [], models: [] };

/**
 * Personalization tab: user-authored text appended to the base system prompt,
 * at three scopes — global, per provider, or per provider+model. The provider
 * and model pickers come from the installed connectors; every applicable scope
 * is appended (broad → specific) at runtime, so they stack rather than replace.
 */
export function PersonalizationTab(props: {
  providers: ConnectorProviderSummary[];
  onError: (message: string) => void;
  onStatus: (message: string) => void;
}) {
  const [settings, setSettings] = createSignal<PersonalizationSettings>(EMPTY);
  const [scope, setScope] = createSignal<ScopeKind>("global");
  const [providerId, setProviderId] = createSignal<string>("");
  const [modelId, setModelId] = createSignal<string>("");
  const [draft, setDraft] = createSignal("");
  const [saving, setSaving] = createSignal(false);

  const providerOptions = () => props.providers;
  const modelOptions = () =>
    props.providers.find((provider) => provider.id === providerId())?.models ??
    [];

  // The currently-stored text for the active scope selection.
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
  const resetDraft = () => setDraft(storedContent());

  onMount(async () => {
    try {
      const loaded = await getPersonalization();
      setSettings(loaded);
      // Default the pickers to the first installed provider/model.
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
    setSaving(true);
    try {
      const updated = await setPersonalization(scopeProvider, scopeModel, content);
      setSettings(updated);
      setDraft(content.trim());
      props.onStatus(
        content.trim()
          ? "Custom instructions saved."
          : "Custom instructions cleared.",
      );
    } catch (error) {
      props.onError(errorMessage(error));
    } finally {
      setSaving(false);
    }
  }

  const canEditScopedScope = () =>
    scope() === "global" ||
    (scope() === "provider" && Boolean(providerId())) ||
    (scope() === "model" && Boolean(providerId()) && Boolean(modelId()));

  const scopeLabel = () => {
    if (scope() === "global") {
      return "Global — applied to every model";
    }
    const provider = props.providers.find((item) => item.id === providerId());
    if (scope() === "provider") {
      return `Provider — ${provider?.label ?? providerId()}`;
    }
    const model = modelOptions().find((item) => item.id === modelId());
    return `Model — ${provider?.label ?? providerId()} · ${model?.label ?? modelId()}`;
  };

  return (
    <div class="settings-pane__inner">
      <div class="settings-pane__intro">
        <h2>Personalization</h2>
        <p>
          Append your own instructions to Mothership's system prompt. Set a
          global baseline, then layer more specific overrides per provider or per
          model — all applicable scopes are added together, broad to specific.
        </p>
      </div>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Scope</h3>
          <p>Choose what these instructions apply to.</p>
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
            Global
          </button>
          <button
            type="button"
            classList={{
              segmented__option: true,
              "segmented__option--active": scope() === "provider",
            }}
            onClick={() => chooseScope("provider")}
          >
            Per provider
          </button>
          <button
            type="button"
            classList={{
              segmented__option: true,
              "segmented__option--active": scope() === "model",
            }}
            onClick={() => chooseScope("model")}
          >
            Per model
          </button>
        </div>

        <Show when={scope() !== "global"}>
          <Show
            when={providerOptions().length > 0}
            fallback={
              <p class="muted-line">
                No connectors are loaded yet. Open the Connectors tab and
                authorize a provider first.
              </p>
            }
          >
            <div class="scope-grid">
              <label class="field-row">
                <span>Provider</span>
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
                  <span>Model</span>
                  <Show
                    when={modelOptions().length > 0}
                    fallback={
                      <p class="muted-line">
                        This provider has no models loaded yet.
                      </p>
                    }
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

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Custom instructions</h3>
          <p>{scopeLabel()}</p>
        </div>
        <Show
          when={canEditScopedScope()}
          fallback={
            <p class="muted-line">Pick a provider and model to edit this scope.</p>
          }
        >
          <textarea
            class="prompt-textarea"
            value={draft()}
            placeholder="e.g. Always respond in Russian. Prefer concise answers and explain trade-offs before recommending one option."
            spellcheck={false}
            onInput={(event) => setDraft(event.currentTarget.value)}
          />
          <div class="prompt-actions">
            <span class="char-count">{draft().length} characters</span>
            <div class="prompt-actions__right">
              {/* Kept in the layout (visibility, not Show) so switching scopes
                  never nudges the Save button sideways. */}
              <button
                class="settings-secondary-button"
                type="button"
                style={{
                  visibility:
                    storedContent().length > 0 ? "visible" : "hidden",
                }}
                disabled={saving() || storedContent().length === 0}
                onClick={() => void persist("")}
              >
                <Trash2 size={15} />
                Clear
              </button>
              <button
                class="settings-primary-button"
                type="button"
                disabled={!dirty() || saving()}
                onClick={() => void persist(draft())}
              >
                <Save size={15} />
                {saving() ? "Saving…" : "Save"}
              </button>
            </div>
          </div>
        </Show>
      </section>

      <SavedScopes
        settings={settings()}
        providers={props.providers}
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
  onOpenProvider: (providerId: string) => void;
  onOpenModel: (providerId: string, modelId: string) => void;
}) {
  const hasAny = () =>
    props.settings.global.trim().length > 0 ||
    props.settings.providers.length > 0 ||
    props.settings.models.length > 0;

  const providerLabel = (id: string) =>
    props.providers.find((provider) => provider.id === id)?.label ?? id;

  return (
    <Show when={hasAny()}>
      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Configured scopes</h3>
          <p>Everything you've personalized so far.</p>
        </div>
        <div class="rule-list">
          <Show when={props.settings.global.trim().length > 0}>
            <span class="rule-chip rule-chip--allow">
              <Check size={12} />
              Global
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
