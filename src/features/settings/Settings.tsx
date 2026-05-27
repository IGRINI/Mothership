import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  Check,
  ChevronLeft,
  Cpu,
  ExternalLink,
  Plug,
  RefreshCw,
  Shield,
  Unplug,
} from "lucide-solid";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

import {
  AdapterSettingsView,
  AuthSession,
  ConnectorProviderSummary,
  ConnectorSettingsSnapshot,
  completeProviderAuth,
  disconnectProviderConnection,
  getConnectorSettings,
  saveAdapterSettings,
  setSelectedModel,
  startProviderOAuthLogin,
} from "../../shared/api/mothership";

export function Settings(props: { onBack: () => void }) {
  const [settings, setSettings] = createSignal<ConnectorSettingsSnapshot>();
  const [pendingSession, setPendingSession] = createSignal<AuthSession>();
  const [callbackUrl, setCallbackUrl] = createSignal("");
  const [connectingProviderId, setConnectingProviderId] =
    createSignal<string>();
  const [error, setError] = createSignal("");
  const [status, setStatus] = createSignal("");
  const [isLoading, setIsLoading] = createSignal(true);

  let unlistenAuthCompleted: (() => void) | undefined;
  let unlistenAuthFailed: (() => void) | undefined;

  onMount(() => {
    void reloadSettings();

    if (!isTauriRuntime()) {
      return;
    }

    void listen("connector-auth-completed", () => {
      setPendingSession(undefined);
      setConnectingProviderId(undefined);
      setStatus("Connector authorized.");
      void reloadSettings();
    }).then((unlisten) => {
      unlistenAuthCompleted = unlisten;
    });

    void listen<string>("connector-auth-failed", (event) => {
      setPendingSession(undefined);
      setConnectingProviderId(undefined);
      setStatus("");
      setError(event.payload);
      void reloadSettings();
    }).then((unlisten) => {
      unlistenAuthFailed = unlisten;
    });
  });

  onCleanup(() => {
    unlistenAuthCompleted?.();
    unlistenAuthFailed?.();
  });

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

  async function connect(provider: ConnectorProviderSummary) {
    if (connectingProviderId() || pendingSession()) {
      setStatus("Authorization is already in progress.");
      return;
    }

    const method = provider.authMethods[0];
    if (!method) {
      setError("This connector has no auth method yet.");
      return;
    }

    setConnectingProviderId(provider.id);
    setError("");
    setStatus("Waiting for browser authorization...");

    try {
      const session = await startProviderOAuthLogin(provider.id, method.id);
      setPendingSession(session);

      const authorizationUrl = session.nextAction.authorizationUrl;
      if (authorizationUrl) {
        try {
          await openAuthUrl(authorizationUrl);
        } catch (caughtError) {
          setError(
            `Authorization started, but browser open failed: ${errorMessage(caughtError)}`,
          );
        }
      }

      void pollUntilConnected(provider.id);
    } catch (caughtError) {
      setConnectingProviderId(undefined);
      setStatus("");
      setError(errorMessage(caughtError));
    }
  }

  async function completeManualCallback() {
    const session = pendingSession();
    if (!session || !callbackUrl().trim()) {
      return;
    }

    setError("");
    setStatus("Completing authorization...");

    try {
      setSettings(await completeProviderAuth(session.id, callbackUrl().trim()));
      setPendingSession(undefined);
      setConnectingProviderId(undefined);
      setCallbackUrl("");
      setStatus("Connector authorized.");
    } catch (caughtError) {
      setStatus("");
      setError(errorMessage(caughtError));
    }
  }

  async function disconnect(connectionId: string) {
    setError("");

    try {
      setSettings(await disconnectProviderConnection(connectionId));
      setStatus("Connector disconnected.");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
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

  async function pollUntilConnected(providerId: string) {
    const deadline = Date.now() + 300_000;

    while (Date.now() < deadline) {
      await delay(1_000);
      const next = await getConnectorSettings();
      setSettings(next);
      const provider = next.providers.find((item) => item.id === providerId);
      if (
        provider?.connections.some(
          (connection) => connection.status === "active",
        )
      ) {
        setStatus("Connector authorized.");
        setPendingSession(undefined);
        setConnectingProviderId(undefined);
        return;
      }
    }

    setConnectingProviderId(undefined);
    setPendingSession(undefined);
    setStatus("Browser authorization is still pending.");
  }

  return (
    <main class="settings-shell">
      <header class="settings-header" data-tauri-drag-region>
        <button class="settings-back" type="button" onClick={props.onBack}>
          <ChevronLeft size={17} />
          Back
        </button>
        <div>
          <h1>Settings</h1>
          <span>Connectors, authorization, and model routing</span>
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
                Provider auth stays in Core. UI receives only safe connection
                metadata.
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
            <div class="connector-grid">
              <For each={settings()?.providers ?? []}>
                {(provider) => (
                  <ConnectorCard
                    isConnecting={connectingProviderId() === provider.id}
                    provider={provider}
                    selectedModelId={settings()?.selectedModel.modelId}
                    onConnect={() => void connect(provider)}
                    onDisconnect={(connectionId) =>
                      void disconnect(connectionId)
                    }
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
        </section>

        <Show when={pendingSession()}>
          {(session) => (
            <section class="settings-section settings-section--auth">
              <div class="settings-section__header">
                <div>
                  <h2>OAuth Callback</h2>
                  <p>
                    Automatic callback listener is running on 127.0.0.1:1455.
                  </p>
                </div>
                <span class="settings-pill">{session().status}</span>
              </div>
              <div class="callback-box">
                <p>
                  If the browser cannot return to Mothership automatically,
                  paste the full callback URL here.
                </p>
                <textarea
                  rows={3}
                  value={callbackUrl()}
                  placeholder="http://localhost:1455/auth/callback?code=...&state=..."
                  onInput={(event) => setCallbackUrl(event.currentTarget.value)}
                />
                <button
                  class="settings-primary-button"
                  type="button"
                  disabled={!callbackUrl().trim()}
                  onClick={() => void completeManualCallback()}
                >
                  Complete Authorization
                </button>
              </div>
            </section>
          )}
        </Show>

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
  isConnecting: boolean;
  onConnect: () => void;
  onDisconnect: (connectionId: string) => void;
  onSelectModel: (modelId: string) => void;
  onSaveSettings: (values: Record<string, string>) => void;
  provider: ConnectorProviderSummary;
  selectedModelId?: string;
}) {
  const provider = () => props.provider;
  const activeConnection = () =>
    provider().connections.find((connection) => connection.status === "active");

  return (
    <article class="connector-card">
      <div class="connector-card__header">
        <span class="connector-icon">
          <Plug size={18} />
        </span>
        <div>
          <h3>{provider().label}</h3>
          <span>{connectorStatusLabel(provider().status)}</span>
        </div>
        <Show
          when={activeConnection()}
          fallback={
            <button
              class="settings-primary-button"
              type="button"
              disabled={
                provider().authMethods.length === 0 || props.isConnecting
              }
              onClick={props.onConnect}
            >
              <Show
                when={!props.isConnecting}
                fallback={
                  <span class="spinner spinner--inline" aria-hidden="true" />
                }
              >
                <ExternalLink size={15} />
              </Show>
              {props.isConnecting ? "Waiting..." : "Connect"}
            </button>
          }
        >
          {(connection) => (
            <button
              class="settings-secondary-button"
              type="button"
              onClick={() => props.onDisconnect(connection().id)}
            >
              <Unplug size={15} />
              Disconnect
            </button>
          )}
        </Show>
      </div>

      <Show when={activeConnection()}>
        {(connection) => (
          <div class="connection-summary">
            <Check size={15} />
            <span>
              <strong>
                {connection().accountLabel ?? "Connected account"}
              </strong>
              <small>
                {connection().accountEmail ?? connection().authMethodId}
              </small>
            </span>
          </div>
        )}
      </Show>

      <div class="connector-card__block">
        <h4>Auth methods</h4>
        <For
          each={provider().authMethods}
          fallback={<p class="muted-line">No auth adapter implemented yet.</p>}
        >
          {(method) => (
            <div class="auth-method-row">
              <span>{method.kind}</span>
              <strong>{method.label}</strong>
            </div>
          )}
        </For>
      </div>

      <div class="connector-card__block">
        <h4>{provider().settingsSchema.modelManagement.title}</h4>
        <div class="model-list">
          <For
            each={provider().models}
            fallback={<p class="muted-line">No models loaded.</p>}
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
          <AdapterSettingsForm
            view={settings()}
            onSave={props.onSaveSettings}
          />
        )}
      </Show>
    </article>
  );
}

function AdapterSettingsForm(props: {
  view: AdapterSettingsView;
  onSave: (values: Record<string, string>) => void;
}) {
  const [values, setValues] = createSignal<Record<string, string>>({
    ...props.view.values,
  });
  const setField = (key: string, value: string) =>
    setValues((current) => ({ ...current, [key]: value }));

  return (
    <div class="connector-card__block">
      <h4>Settings</h4>
      <div class="adapter-settings">
        <For
          each={props.view.fields}
          fallback={<p class="muted-line">No settings.</p>}
        >
          {(field) => (
            <label class="adapter-setting">
              <span>
                {field.label}
                {field.required ? " *" : ""}
              </span>
              <Show
                when={field.kind === "bool"}
                fallback={
                  <input
                    type={field.kind === "secret" ? "password" : "text"}
                    value={values()[field.key] ?? ""}
                    onInput={(event) =>
                      setField(field.key, event.currentTarget.value)
                    }
                  />
                }
              >
                <input
                  type="checkbox"
                  checked={values()[field.key] === "true"}
                  onChange={(event) =>
                    setField(
                      field.key,
                      event.currentTarget.checked ? "true" : "false",
                    )
                  }
                />
              </Show>
            </label>
          )}
        </For>
        <button
          class="settings-primary-button"
          type="button"
          onClick={() => props.onSave(values())}
        >
          Save settings
        </button>
      </div>
    </div>
  );
}

async function openAuthUrl(url: string) {
  if (isTauriRuntime()) {
    await openUrl(url);
    return;
  }

  window.open(url, "_blank", "noopener,noreferrer");
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function connectorStatusLabel(status: string) {
  if (status === "connected") {
    return "Connected";
  }

  if (status === "not_available") {
    return "Adapter pending";
  }

  return "Not connected";
}

function delay(ms: number) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
