import { createMemo, createSignal, For, Index, Show } from "solid-js";
import {
  ChevronLeft,
  Cpu,
  LogIn,
  LogOut,
  Plug,
  Plus,
  Settings2,
  X,
} from "lucide-solid";

import type {
  AdapterSettingsField,
  AdapterSettingPatchValue,
  AdapterSettingsView,
  ConnectorProviderSummary,
} from "../../../shared/api/mothership";
import { parseSettingsList } from "../../../shared/settings-lists";

interface ConnectorActions {
  onAuthorize: (providerId: string) => void;
  onCancelAuthorize: (providerId: string) => void;
  onLogout: (providerId: string) => void;
  onSelectModel: (providerId: string, modelId: string) => void;
  onSetEnabled: (providerId: string, enabled: boolean) => void;
  onSaveSettings: (
    providerId: string,
    patch: Record<string, AdapterSettingPatchValue>,
  ) => void;
}

type StatusTone = "ok" | "warn" | "error" | "off" | "pending";

interface ProviderStatus {
  tone: StatusTone;
  text: string;
}

/**
 * Connectors tab. A master/detail view: a grid of uniform provider cards (each
 * with an on/off switch and a Settings button), and — when a card's Settings is
 * opened — a full page for that one provider (auth, model selection, and the
 * adapter-declared settings form). Purely presentational; the Settings shell
 * owns loading the snapshot, the event subscription, and the action calls.
 */
export function ConnectorsTab(
  props: {
    loading: boolean;
    providers: ConnectorProviderSummary[];
    authorizingId: string | undefined;
  } & ConnectorActions,
) {
  const [detailId, setDetailId] = createSignal<string>();
  // Re-derive the open provider from the live snapshot so it keeps updating
  // (auth finishing, models loading) while its page is open.
  const detailProvider = createMemo(() =>
    props.providers.find((provider) => provider.id === detailId()),
  );

  return (
    <div class="settings-pane__inner">
      <Show
        when={detailProvider()}
        fallback={
          <ConnectorOverview
            loading={props.loading}
            providers={props.providers}
            authorizingId={props.authorizingId}
            onOpen={setDetailId}
            onSetEnabled={props.onSetEnabled}
          />
        }
      >
        {(provider) => (
          <ConnectorDetail
            provider={provider()}
            busy={props.authorizingId === provider().id}
            onBack={() => setDetailId(undefined)}
            onAuthorize={() => props.onAuthorize(provider().id)}
            onCancelAuthorize={() => props.onCancelAuthorize(provider().id)}
            onLogout={() => props.onLogout(provider().id)}
            onSetEnabled={(enabled) =>
              props.onSetEnabled(provider().id, enabled)
            }
            onSelectModel={(modelId) =>
              props.onSelectModel(provider().id, modelId)
            }
            onSaveSettings={(patch) =>
              props.onSaveSettings(provider().id, patch)
            }
          />
        )}
      </Show>
    </div>
  );
}

function ConnectorOverview(props: {
  loading: boolean;
  providers: ConnectorProviderSummary[];
  authorizingId: string | undefined;
  onOpen: (providerId: string) => void;
  onSetEnabled: (providerId: string, enabled: boolean) => void;
}) {
  return (
    <>
      <div class="settings-pane__intro">
        <h2>Connectors</h2>
        <p>
          Each provider is a runtime-loaded adapter that authorizes itself. Turn
          one off to hide it from model selection without losing its setup; open
          Settings to authorize, pick models, and configure it.
        </p>
      </div>

      <Show
        when={!props.loading}
        fallback={<div class="settings-empty">Loading connectors…</div>}
      >
        <Show
          when={props.providers.length > 0}
          fallback={
            <div class="settings-empty">
              No connectors found. Restart the app; if this keeps happening,
              reinstall Mothership.
            </div>
          }
        >
          <div class="provider-grid">
            <Index each={props.providers}>
              {(provider) => (
                <ProviderCard
                  provider={provider()}
                  busy={props.authorizingId === provider().id}
                  onOpen={() => props.onOpen(provider().id)}
                  onSetEnabled={(enabled) =>
                    props.onSetEnabled(provider().id, enabled)
                  }
                />
              )}
            </Index>
          </div>
        </Show>
      </Show>
    </>
  );
}

function ProviderCard(props: {
  provider: ConnectorProviderSummary;
  busy: boolean;
  onOpen: () => void;
  onSetEnabled: (enabled: boolean) => void;
}) {
  const provider = () => props.provider;
  const status = createMemo(() => providerStatus(provider()));
  const modelCount = () => provider().models.length;

  return (
    <article
      classList={{
        "provider-card": true,
        "provider-card--off": !provider().enabled,
      }}
    >
      <div class="provider-card__top">
        <span class="connector-icon">
          <Show when={provider().icon} fallback={<Plug size={18} />}>
            {(icon) => <img src={icon()} alt="" class="connector-icon__img" />}
          </Show>
        </span>
        <div class="provider-card__id">
          <h3 title={provider().label}>{provider().label}</h3>
          <span class="provider-card__sub">
            {modelCount() > 0
              ? `${modelCount()} ${modelCount() === 1 ? "model" : "models"}`
              : runtimeKindLabel(provider())}
          </span>
        </div>
        <ToggleSwitch
          checked={provider().enabled}
          disabled={props.busy}
          label={`Enable ${provider().label}`}
          onChange={props.onSetEnabled}
        />
      </div>

      <div class="provider-card__foot">
        <span class="provider-status">
          <span class={`status-dot status-dot--${status().tone}`} />
          <span class="provider-status__text">{status().text}</span>
        </span>
        <button
          class="provider-card__settings"
          type="button"
          onClick={props.onOpen}
        >
          <Settings2 size={15} />
          Settings
        </button>
      </div>
    </article>
  );
}

function ConnectorDetail(
  props: {
    provider: ConnectorProviderSummary;
    busy: boolean;
    onBack: () => void;
    onSetEnabled: (enabled: boolean) => void;
  } & {
    onAuthorize: () => void;
    onCancelAuthorize: () => void;
    onLogout: () => void;
    onSelectModel: (modelId: string) => void;
    onSaveSettings: (patch: Record<string, AdapterSettingPatchValue>) => void;
  },
) {
  const provider = () => props.provider;
  const status = createMemo(() => providerStatus(provider()));
  const needsAuthorize = () =>
    provider().authKind === "oauth_internal" ||
    provider().authKind === "external_process";
  const selectedModelId = () => provider().selectedModelId ?? undefined;

  return (
    <div class="connector-detail">
      <button class="connector-detail__back" type="button" onClick={props.onBack}>
        <ChevronLeft size={16} />
        Connectors
      </button>

      <header class="connector-detail__hero">
        <span class="connector-icon connector-icon--lg">
          <Show when={provider().icon} fallback={<Plug size={22} />}>
            {(icon) => <img src={icon()} alt="" class="connector-icon__img" />}
          </Show>
        </span>
        <div class="connector-detail__title">
          <h2>{provider().label}</h2>
          <span class="provider-status">
            <span class={`status-dot status-dot--${status().tone}`} />
            <span class="provider-status__text">{status().text}</span>
          </span>
        </div>
        <label class="connector-detail__power">
          <span>{provider().enabled ? "On" : "Off"}</span>
          <ToggleSwitch
            checked={provider().enabled}
            disabled={props.busy}
            label={`Enable ${provider().label}`}
            onChange={props.onSetEnabled}
          />
        </label>
      </header>

      <Show when={!provider().enabled}>
        <p class="connector-detail__hint">
          This connector is turned off, so it won't appear when choosing a model.
          Turn it on to use it in chat — your setup below is kept either way.
        </p>
      </Show>

      <Show when={needsAuthorize()}>
        <section class="connector-detail__section">
          <h3>Authorization</h3>
          <div class="connector-auth">
            <p class="muted-line">{authDetail(provider())}</p>
            <Show
              when={props.busy}
              fallback={
                <Show
                  when={provider().authenticated}
                  fallback={
                    <button
                      class="settings-primary-button"
                      type="button"
                      onClick={props.onAuthorize}
                    >
                      <LogIn size={15} />
                      Authorize
                    </button>
                  }
                >
                  <button
                    class="settings-secondary-button"
                    type="button"
                    onClick={props.onLogout}
                  >
                    <LogOut size={15} />
                    Log out
                  </button>
                </Show>
              }
            >
              <button
                class="settings-secondary-button"
                type="button"
                onClick={props.onCancelAuthorize}
              >
                <X size={15} />
                Cancel
              </button>
            </Show>
          </div>
        </section>
      </Show>

      <section class="connector-detail__section">
        <h3>{provider().settingsSchema.modelManagement.title}</h3>
        <Show when={provider().modelError}>
          {(modelError) => (
            <p class="muted-line">Connector unavailable: {modelError()}</p>
          )}
        </Show>
        <div class="model-list">
          <For
            each={provider().models}
            fallback={
              <p class="muted-line">
                {provider().refreshStatus === "refreshing" ||
                provider().refreshStatus === "pending"
                  ? "Loading models…"
                  : needsAuthorize()
                    ? "No models loaded — authorize to fetch them."
                    : "No models loaded."}
              </p>
            }
          >
            {(model) => (
              <label
                classList={{
                  "model-option": true,
                  "model-option--selected": selectedModelId() === model.id,
                }}
              >
                <input
                  checked={selectedModelId() === model.id}
                  name={`model-${provider().id}`}
                  type="radio"
                  onChange={() => props.onSelectModel(model.id)}
                />
                <span>
                  <strong>
                    <Cpu size={14} />
                    {model.label}
                  </strong>
                  <small>{model.description}</small>
                </span>
              </label>
            )}
          </For>
        </div>
      </section>

      <Show when={provider().adapterSettings}>
        {(settings) => (
          <Show when={settings().fields.length > 0}>
            <section class="connector-detail__section">
              <h3>Settings</h3>
              <AdapterSettingsForm
                view={settings()}
                onSave={props.onSaveSettings}
              />
            </section>
          </Show>
        )}
      </Show>
    </div>
  );
}

function ToggleSwitch(props: {
  checked: boolean;
  disabled?: boolean;
  label: string;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={props.checked}
      aria-label={props.label}
      classList={{ switch: true, "switch--on": props.checked }}
      disabled={props.disabled}
      onClick={() => props.onChange(!props.checked)}
    >
      <span class="switch__thumb" />
    </button>
  );
}

function runtimeKindLabel(provider: ConnectorProviderSummary) {
  return provider.runtimeKind === "self_managed" ? "Agent runtime" : "Connector";
}

function providerStatus(provider: ConnectorProviderSummary): ProviderStatus {
  if (!provider.enabled) {
    return { tone: "off", text: "Disabled" };
  }
  if (provider.refreshStatus === "refreshing") {
    return { tone: "pending", text: "Updating models…" };
  }
  if (provider.modelError) {
    return { tone: "error", text: "Unavailable" };
  }

  const needsAuthorize =
    provider.authKind === "oauth_internal" ||
    provider.authKind === "external_process";
  if (needsAuthorize && !provider.authenticated) {
    return { tone: "warn", text: "Not connected" };
  }
  if (provider.authKind === "api_key" && !provider.authenticated) {
    return { tone: "warn", text: "Needs API key" };
  }

  const accountLabel = provider.authStatus.accountLabel;
  if (provider.authenticated) {
    return {
      tone: "ok",
      text: accountLabel ? `Connected · ${accountLabel}` : "Connected",
    };
  }
  if (provider.refreshStatus === "pending") {
    return { tone: "pending", text: "Waiting for connector…" };
  }
  return { tone: "ok", text: "Ready" };
}

function authDetail(provider: ConnectorProviderSummary) {
  const status = provider.authStatus;
  if (status.accountLabel) {
    return `Signed in as ${status.accountLabel}.`;
  }
  if (status.detail) {
    return status.detail;
  }
  return provider.authenticated
    ? "Authorized."
    : "Authorize this connector to fetch its models.";
}

function AdapterSettingsForm(props: {
  view: AdapterSettingsView;
  onSave: (patch: Record<string, AdapterSettingPatchValue>) => void;
}) {
  // Secret inputs intentionally start empty: the backend returns only sanitized
  // metadata, and an empty secret input means "leave existing value unchanged".
  const scalarInit: Record<string, string> = {};
  const secretInit: Record<string, string> = {};
  const secretClearInit: Record<string, boolean> = {};
  const listInit: Record<string, string[]> = {};
  for (const field of props.view.fields) {
    if (field.kind === "string_list" || field.kind === "model_visibility_list") {
      listInit[field.key] = parseSettingsList(props.view.values[field.key] ?? "");
    } else if (field.kind === "secret") {
      secretInit[field.key] = "";
      secretClearInit[field.key] = false;
    } else if (field.kind === "bool") {
      scalarInit[field.key] =
        props.view.values[field.key] === "true" ? "true" : "false";
    } else {
      scalarInit[field.key] = props.view.values[field.key] ?? "";
    }
  }

  const [scalars, setScalars] = createSignal<Record<string, string>>(scalarInit);
  const [secrets, setSecrets] = createSignal<Record<string, string>>(secretInit);
  const [secretClears, setSecretClears] =
    createSignal<Record<string, boolean>>(secretClearInit);
  const [lists, setLists] = createSignal<Record<string, string[]>>(listInit);

  const setScalar = (key: string, value: string) =>
    setScalars((current) => ({ ...current, [key]: value }));
  const setSecret = (key: string, value: string) => {
    setSecrets((current) => ({ ...current, [key]: value }));
    if (value.length > 0) {
      setSecretClears((current) => ({ ...current, [key]: false }));
    }
  };
  const setSecretClear = (key: string, checked: boolean) => {
    setSecretClears((current) => ({ ...current, [key]: checked }));
    if (checked) {
      setSecrets((current) => ({ ...current, [key]: "" }));
    }
  };
  const setListItem = (key: string, index: number, value: string) =>
    setLists((current) => {
      const next = [...(current[key] ?? [])];
      next[index] = value;
      return { ...current, [key]: next };
    });
  const addListItem = (key: string) =>
    setLists((current) => ({ ...current, [key]: [...(current[key] ?? []), ""] }));
  const removeListItem = (key: string, index: number) =>
    setLists((current) => ({
      ...current,
      [key]: (current[key] ?? []).filter((_, i) => i !== index),
    }));
  const setModelVisible = (
    field: AdapterSettingsField,
    modelId: string,
    visible: boolean,
  ) =>
    setLists((current) => {
      const hidden = new Set(current[field.key] ?? []);
      if (visible) {
        hidden.delete(modelId);
      } else {
        hidden.add(modelId);
      }

      const optionValues = field.options.map((option) => option.value);
      const orderedKnown = optionValues.filter((value) => hidden.has(value));
      const unknown = [...hidden].filter((value) => !optionValues.includes(value));
      return { ...current, [field.key]: [...orderedKnown, ...unknown] };
    });

  function submit() {
    const patch: Record<string, AdapterSettingPatchValue> = {};
    for (const field of props.view.fields) {
      if (field.kind === "string_list" || field.kind === "model_visibility_list") {
        patch[field.key] = {
          action: "set",
          value: (lists()[field.key] ?? [])
            .map((item) => item.trim())
            .filter(Boolean)
            .join("\n"),
        };
      } else if (field.kind === "secret") {
        const value = secrets()[field.key] ?? "";
        patch[field.key] =
          value.length > 0
            ? { action: "set", value }
            : secretClears()[field.key]
              ? { action: "clear" }
              : { action: "unchanged" };
      } else {
        patch[field.key] = {
          action: "set",
          value: scalars()[field.key] ?? "",
        };
      }
    }
    props.onSave(patch);
  }

  function secretDescription(key: string) {
    const state = props.view.secrets?.[key];
    if (!state?.hasValue) {
      return "No secret saved.";
    }

    return state.last4
      ? `Saved secret ending in ${state.last4}. Leave empty to keep it.`
      : "Saved secret configured. Leave empty to keep it.";
  }

  function hasSavedSecret(key: string) {
    return props.view.secrets?.[key]?.hasValue ?? false;
  }

  return (
    <div class="adapter-settings">
      <Index each={props.view.fields}>
        {(field) => (
          <Show
            when={field().kind === "model_visibility_list"}
            fallback={
              <Show
                when={field().kind === "string_list"}
                fallback={
                  <Show
                    when={field().kind === "bool"}
                    fallback={
                      <Show
                        when={field().kind === "secret"}
                        fallback={
                          <label class="adapter-setting">
                            <span>
                              {field().label}
                              {field().required ? " *" : ""}
                            </span>
                            <input
                              type="text"
                              value={scalars()[field().key] ?? ""}
                              onInput={(event) =>
                                setScalar(field().key, event.currentTarget.value)
                              }
                            />
                          </label>
                        }
                      >
                        <div class="adapter-setting">
                          <span>
                            {field().label}
                            {field().required ? " *" : ""}
                          </span>
                          <input
                            aria-label={field().label}
                            type="password"
                            value={secrets()[field().key] ?? ""}
                            placeholder={
                              hasSavedSecret(field().key)
                                ? "Leave blank to keep saved secret"
                                : ""
                            }
                            onInput={(event) =>
                              setSecret(field().key, event.currentTarget.value)
                            }
                          />
                          <p class="muted-line">
                            {secretDescription(field().key)}
                          </p>
                          <Show when={hasSavedSecret(field().key)}>
                            <label class="adapter-setting adapter-setting--bool">
                              <input
                                type="checkbox"
                                checked={secretClears()[field().key] ?? false}
                                onChange={(event) =>
                                  setSecretClear(
                                    field().key,
                                    event.currentTarget.checked,
                                  )
                                }
                              />
                              <span>Clear saved secret</span>
                            </label>
                          </Show>
                        </div>
                      </Show>
                    }
                  >
                    <label class="adapter-setting adapter-setting--bool">
                      <input
                        type="checkbox"
                        checked={scalars()[field().key] === "true"}
                        onChange={(event) =>
                          setScalar(
                            field().key,
                            event.currentTarget.checked ? "true" : "false",
                          )
                        }
                      />
                      <span>
                        {field().label}
                        {field().required ? " *" : ""}
                      </span>
                    </label>
                  </Show>
                }
              >
                <div class="adapter-setting">
                  <span>
                    {field().label}
                    {field().required ? " *" : ""}
                  </span>
                  <div class="adapter-list">
                    <Index
                      each={lists()[field().key] ?? []}
                      fallback={<p class="muted-line">None yet.</p>}
                    >
                      {(item, index) => (
                        <div class="adapter-list__row">
                          <input
                            type="text"
                            value={item()}
                            placeholder="provider/model-id"
                            onInput={(event) =>
                              setListItem(
                                field().key,
                                index,
                                event.currentTarget.value,
                              )
                            }
                          />
                          <button
                            class="adapter-list__remove"
                            type="button"
                            aria-label="Remove"
                            onClick={() => removeListItem(field().key, index)}
                          >
                            <X size={14} />
                          </button>
                        </div>
                      )}
                    </Index>
                    <button
                      class="adapter-list__add"
                      type="button"
                      onClick={() => addListItem(field().key)}
                    >
                      <Plus size={14} />
                      Add model
                    </button>
                  </div>
                </div>
              </Show>
            }
          >
            <div class="adapter-setting">
              <span>
                {field().label}
                {field().required ? " *" : ""}
              </span>
              <div class="adapter-visibility-list">
                <Index
                  each={field().options}
                  fallback={<p class="muted-line">None yet.</p>}
                >
                  {(option) => (
                    <label class="adapter-visibility-row">
                      <input
                        type="checkbox"
                        checked={
                          !(lists()[field().key] ?? []).includes(option().value)
                        }
                        onChange={(event) =>
                          setModelVisible(
                            field(),
                            option().value,
                            event.currentTarget.checked,
                          )
                        }
                      />
                      <span>{option().label}</span>
                    </label>
                  )}
                </Index>
              </div>
            </div>
          </Show>
        )}
      </Index>
      <button class="settings-primary-button" type="button" onClick={submit}>
        Save settings
      </button>
    </div>
  );
}
