import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  ChevronLeft,
  Cpu,
  LogIn,
  LogOut,
  Plug,
  Plus,
  RefreshCw,
  Shield,
  X,
} from "lucide-solid";

import {
  AdapterSettingsView,
  ConnectorProviderSummary,
  ConnectorSettingsSnapshot,
  authenticateAdapter,
  cancelAuthenticateAdapter,
  getConnectorSettings,
  logoutAdapter,
  saveAdapterSettings,
  setSelectedModel,
} from "../../shared/api/mothership";

export function Settings(props: { onBack: () => void }) {
  const [settings, setSettings] = createSignal<ConnectorSettingsSnapshot>();
  const [error, setError] = createSignal("");
  const [status, setStatus] = createSignal("");
  const [isLoading, setIsLoading] = createSignal(true);
  const [authorizingId, setAuthorizingId] = createSignal<string>();

  onMount(() => {
    void reloadSettings();
  });

  // Leaving Settings while an authorization is in flight cancels it (kills the
  // adapter process), so an abandoned browser flow doesn't linger.
  onCleanup(() => {
    const inFlight = authorizingId();
    if (inFlight) {
      void cancelAuthenticateAdapter(inFlight);
    }
  });

  function goBack() {
    const inFlight = authorizingId();
    if (inFlight) {
      void cancelAuthenticateAdapter(inFlight);
    }
    props.onBack();
  }

  async function reloadSettings() {
    setIsLoading(true);
    setError("");

    try {
      setSettings(await getConnectorSettings());
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsLoading(false);
    }
  }

  async function selectModel(providerId: string, modelId: string) {
    setError("");

    try {
      setSettings(await setSelectedModel(providerId, modelId));
      setStatus("Model selection saved.");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function saveAdapter(
    providerId: string,
    values: Record<string, string>,
  ) {
    setError("");

    try {
      setSettings(await saveAdapterSettings(providerId, values));
      setStatus("Adapter settings saved.");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function authorize(providerId: string) {
    if (authorizingId()) {
      return;
    }
    setError("");
    setStatus("Authorizing — finish the flow in your browser...");
    setAuthorizingId(providerId);

    try {
      const snapshot = await authenticateAdapter(providerId);
      setSettings(snapshot);
      // The command returns cleanly whether the flow completed or was cancelled;
      // the snapshot tells us which.
      const provider = snapshot.providers.find((item) => item.id === providerId);
      setStatus(provider?.authenticated ? "Authorized." : "Authorization cancelled.");
    } catch (caughtError) {
      setStatus("");
      setError(errorMessage(caughtError));
    } finally {
      setAuthorizingId(undefined);
    }
  }

  async function cancelAuthorize(providerId: string) {
    setError("");
    setStatus("Cancelling authorization...");
    try {
      // Terminates the adapter process; the in-flight authorize() above then
      // resolves and refreshes the snapshot.
      await cancelAuthenticateAdapter(providerId);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function logout(providerId: string) {
    if (authorizingId()) {
      return;
    }
    setError("");
    setAuthorizingId(providerId);

    try {
      setSettings(await logoutAdapter(providerId));
      setStatus("Logged out.");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setAuthorizingId(undefined);
    }
  }

  return (
    <main class="settings-shell">
      <header class="settings-header" data-tauri-drag-region>
        <button class="settings-back" type="button" onClick={goBack}>
          <ChevronLeft size={17} />
          Back
        </button>
        <div>
          <h1>Settings</h1>
          <span>Connectors and model routing</span>
        </div>
        <button
          class="icon-button icon-button--ghost"
          type="button"
          title="Refresh"
          onClick={() => void reloadSettings()}
        >
          <RefreshCw size={16} />
        </button>
      </header>

      <div class="settings-content">
        <section class="settings-section">
          <div class="settings-section__header">
            <div>
              <h2>Connectors</h2>
              <p>
                Each provider is a runtime-loaded adapter that authorizes
                itself. Secrets it stores live in the app's shared vault.
              </p>
            </div>
            <span class="settings-pill">
              <Shield size={14} />
              Vault-backed
            </span>
          </div>

          <Show
            when={!isLoading()}
            fallback={<div class="settings-empty">Loading settings...</div>}
          >
            <Show
              when={(settings()?.providers ?? []).length > 0}
              fallback={
                <div class="settings-empty">
                  No connectors installed. Drop an adapter into the app's
                  plugins folder (or run <code>npm run adapters:install</code>)
                  and hit Refresh.
                </div>
              }
            >
              <div class="connector-grid">
                <For each={settings()?.providers ?? []}>
                  {(provider) => (
                    <ConnectorCard
                      provider={provider}
                      selectedModelId={settings()?.selectedModel.modelId}
                      busy={authorizingId() === provider.id}
                      onAuthorize={() => void authorize(provider.id)}
                      onCancelAuthorize={() => void cancelAuthorize(provider.id)}
                      onLogout={() => void logout(provider.id)}
                      onSelectModel={(modelId) =>
                        void selectModel(provider.id, modelId)
                      }
                      onSaveSettings={(values) =>
                        void saveAdapter(provider.id, values)
                      }
                    />
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </section>

        <Show when={error()}>
          <div class="settings-alert settings-alert--error" role="alert">
            {error()}
          </div>
        </Show>
        <Show when={status()}>
          <div class="settings-alert">{status()}</div>
        </Show>
      </div>
    </main>
  );
}

function ConnectorCard(props: {
  busy: boolean;
  onAuthorize: () => void;
  onCancelAuthorize: () => void;
  onLogout: () => void;
  onSelectModel: (modelId: string) => void;
  onSaveSettings: (values: Record<string, string>) => void;
  provider: ConnectorProviderSummary;
  selectedModelId?: string;
}) {
  const provider = () => props.provider;
  const needsAuthorize = () =>
    provider().authKind === "oauth_internal" ||
    provider().authKind === "external_process";

  return (
    <article class="connector-card">
      <div class="connector-card__header">
        <span class="connector-icon">
          <Show when={provider().icon} fallback={<Plug size={18} />}>
            {(icon) => (
              <img src={icon()} alt="" class="connector-icon__img" />
            )}
          </Show>
        </span>
        <div>
          <h3>{provider().label}</h3>
        </div>
        <Show when={needsAuthorize()}>
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
        </Show>
      </div>

      <div class="connector-card__block">
        <h4>{provider().settingsSchema.modelManagement.title}</h4>
        <div class="model-list">
          <For
            each={provider().models}
            fallback={
              <p class="muted-line">
                {needsAuthorize()
                  ? "No models loaded — authorize to fetch them."
                  : "No models loaded."}
              </p>
            }
          >
            {(model) => (
              <label
                classList={{
                  "model-option": true,
                  "model-option--selected": props.selectedModelId === model.id,
                }}
              >
                <input
                  checked={props.selectedModelId === model.id}
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
      </div>

      <Show when={props.provider.adapterSettings}>
        {(settings) => (
          <Show when={settings().fields.length > 0}>
            <AdapterSettingsForm
              view={settings()}
              onSave={props.onSaveSettings}
            />
          </Show>
        )}
      </Show>
    </article>
  );
}

function AdapterSettingsForm(props: {
  view: AdapterSettingsView;
  onSave: (values: Record<string, string>) => void;
}) {
  // Scalar fields (text/secret/bool) live in one record; list fields are edited
  // as string arrays and joined with "\n" on save.
  const scalarInit: Record<string, string> = {};
  const listInit: Record<string, string[]> = {};
  for (const field of props.view.fields) {
    if (field.kind === "string_list") {
      listInit[field.key] = (props.view.values[field.key] ?? "")
        .split(/[\n,]/)
        .map((item) => item.trim())
        .filter(Boolean);
    } else {
      scalarInit[field.key] = props.view.values[field.key] ?? "";
    }
  }

  const [scalars, setScalars] = createSignal<Record<string, string>>(scalarInit);
  const [lists, setLists] = createSignal<Record<string, string[]>>(listInit);

  const setScalar = (key: string, value: string) =>
    setScalars((current) => ({ ...current, [key]: value }));
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

  function submit() {
    const out: Record<string, string> = { ...scalars() };
    for (const field of props.view.fields) {
      if (field.kind === "string_list") {
        out[field.key] = (lists()[field.key] ?? [])
          .map((item) => item.trim())
          .filter(Boolean)
          .join("\n");
      }
    }
    props.onSave(out);
  }

  return (
    <div class="connector-card__block">
      <h4>Settings</h4>
      <div class="adapter-settings">
        <For each={props.view.fields}>
          {(field) => (
            <Show
              when={field.kind === "string_list"}
              fallback={
                <Show
                  when={field.kind === "bool"}
                  fallback={
                    <label class="adapter-setting">
                      <span>
                        {field.label}
                        {field.required ? " *" : ""}
                      </span>
                      <input
                        type={field.kind === "secret" ? "password" : "text"}
                        value={scalars()[field.key] ?? ""}
                        onInput={(event) =>
                          setScalar(field.key, event.currentTarget.value)
                        }
                      />
                    </label>
                  }
                >
                  <label class="adapter-setting adapter-setting--bool">
                    <input
                      type="checkbox"
                      checked={scalars()[field.key] === "true"}
                      onChange={(event) =>
                        setScalar(
                          field.key,
                          event.currentTarget.checked ? "true" : "false",
                        )
                      }
                    />
                    <span>
                      {field.label}
                      {field.required ? " *" : ""}
                    </span>
                  </label>
                </Show>
              }
            >
              <div class="adapter-setting">
                <span>
                  {field.label}
                  {field.required ? " *" : ""}
                </span>
                <div class="adapter-list">
                  <For
                    each={lists()[field.key] ?? []}
                    fallback={<p class="muted-line">None yet.</p>}
                  >
                    {(item, index) => (
                      <div class="adapter-list__row">
                        <input
                          type="text"
                          value={item}
                          placeholder="provider/model-id"
                          onInput={(event) =>
                            setListItem(
                              field.key,
                              index(),
                              event.currentTarget.value,
                            )
                          }
                        />
                        <button
                          class="adapter-list__remove"
                          type="button"
                          aria-label="Remove"
                          onClick={() => removeListItem(field.key, index())}
                        >
                          <X size={14} />
                        </button>
                      </div>
                    )}
                  </For>
                  <button
                    class="adapter-list__add"
                    type="button"
                    onClick={() => addListItem(field.key)}
                  >
                    <Plus size={14} />
                    Add model
                  </button>
                </div>
              </div>
            </Show>
          )}
        </For>
        <button
          class="settings-primary-button"
          type="button"
          onClick={submit}
        >
          Save settings
        </button>
      </div>
    </div>
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
