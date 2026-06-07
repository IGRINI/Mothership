import {
  createMemo,
  createSignal,
  For,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
  type Component,
} from "solid-js";
import { Dynamic, Portal } from "solid-js/web";
import { listen } from "@tauri-apps/api/event";
import {
  ChevronLeft,
  MessageSquare,
  Palette,
  Plug,
  RefreshCw,
  ShieldCheck,
  Sparkles,
  type LucideProps,
} from "lucide-solid";

import type {
  AdapterSettingPatchValue,
  ConnectorSettingsEvent,
  ConnectorSettingsSnapshot,
} from "../../shared/api/mothership";
import {
  authenticateAdapter,
  cancelAuthenticateAdapter,
  getConnectorSettings,
  logoutAdapter,
  saveAdapterSettings,
  setProviderEnabled,
  setSelectedModel,
} from "../../shared/api/mothership";
import { startWindowDrag } from "../../shared/window-drag";
import { AppearanceTab } from "./tabs/AppearanceTab";
import { ChatTab } from "./tabs/ChatTab";
import { ConnectorsTab } from "./tabs/ConnectorsTab";
import { PermissionsTab } from "./tabs/PermissionsTab";
import { PersonalizationTab } from "./tabs/PersonalizationTab";

type TabId =
  | "appearance"
  | "chat"
  | "connectors"
  | "personalization"
  | "permissions";

interface TabDef {
  id: TabId;
  label: string;
  icon: Component<LucideProps>;
}

const NAV_GROUPS: { label: string; items: TabDef[] }[] = [
  {
    label: "General",
    items: [
      { id: "appearance", label: "Appearance", icon: Palette },
      { id: "chat", label: "Chat", icon: MessageSquare },
    ],
  },
  {
    label: "Providers",
    items: [
      { id: "connectors", label: "Connectors", icon: Plug },
      { id: "personalization", label: "Personalization", icon: Sparkles },
    ],
  },
  {
    label: "Tools & safety",
    items: [{ id: "permissions", label: "Permissions", icon: ShieldCheck }],
  },
];

export function Settings(props: { onBack: () => void }) {
  const [tab, setTab] = createSignal<TabId>("appearance");
  const [settings, setSettings] = createSignal<ConnectorSettingsSnapshot>();
  const [error, setError] = createSignal("");
  const [status, setStatus] = createSignal("");
  const [isLoading, setIsLoading] = createSignal(true);
  const [authorizingId, setAuthorizingId] = createSignal<string>();
  let unlistenConnectorSettings: (() => void) | undefined;
  let statusTimer: number | undefined;

  const tabLabel = createMemo(
    () =>
      NAV_GROUPS.flatMap((group) => group.items).find(
        (item) => item.id === tab(),
      )?.label ?? "",
  );

  onMount(() => {
    void reloadSettings();

    if (isTauriRuntime()) {
      let disposed = false;
      void listen<ConnectorSettingsEvent>("connector-settings-event", (event) => {
        setSettings(event.payload.snapshot);
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenConnectorSettings = unlisten;
        }
      });

      onCleanup(() => {
        disposed = true;
      });
    }
  });

  // Leaving Settings while an authorization is in flight cancels it (kills the
  // adapter process), so an abandoned browser flow doesn't linger.
  onCleanup(() => {
    const inFlight = authorizingId();
    if (inFlight) {
      void cancelAuthenticateAdapter(inFlight);
    }
    unlistenConnectorSettings?.();
    if (statusTimer !== undefined) {
      window.clearTimeout(statusTimer);
    }
  });

  function notify(message: string) {
    setError("");
    setStatus(message);
    if (statusTimer !== undefined) {
      window.clearTimeout(statusTimer);
    }
    statusTimer = window.setTimeout(() => setStatus(""), 3200);
  }

  function fail(message: string) {
    setStatus("");
    setError(message);
  }

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
      fail(errorMessage(caughtError));
    } finally {
      setIsLoading(false);
    }
  }

  async function selectModel(providerId: string, modelId: string) {
    setError("");
    try {
      setSettings(await setSelectedModel(providerId, modelId));
      notify("Model selection saved.");
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    }
  }

  async function saveAdapter(
    providerId: string,
    patch: Record<string, AdapterSettingPatchValue>,
  ) {
    setError("");
    try {
      setSettings(await saveAdapterSettings(providerId, patch));
      notify("Adapter settings saved.");
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    }
  }

  async function setEnabled(providerId: string, enabled: boolean) {
    setError("");
    try {
      setSettings(await setProviderEnabled(providerId, enabled));
      notify(enabled ? "Connector enabled." : "Connector disabled.");
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    }
  }

  async function authorize(providerId: string) {
    if (authorizingId()) {
      return;
    }
    setError("");
    notify("Authorizing — finish the flow in your browser…");
    setAuthorizingId(providerId);

    try {
      const snapshot = await authenticateAdapter(providerId);
      setSettings(snapshot);
      const provider = snapshot.providers.find((item) => item.id === providerId);
      notify(provider?.authenticated ? "Authorized." : "Authorization cancelled.");
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    } finally {
      setAuthorizingId(undefined);
    }
  }

  async function cancelAuthorize(providerId: string) {
    setError("");
    try {
      await cancelAuthenticateAdapter(providerId);
    } catch (caughtError) {
      fail(errorMessage(caughtError));
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
      notify("Logged out.");
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    } finally {
      setAuthorizingId(undefined);
    }
  }

  return (
    <main class="settings-shell">
      <header class="settings-header" onMouseDown={startWindowDrag}>
        <button class="settings-back" type="button" onClick={goBack}>
          <ChevronLeft size={17} />
          Back
        </button>
        <div>
          <h1>Settings</h1>
          <span>{tabLabel()}</span>
        </div>
        <Show
          when={tab() === "connectors"}
          fallback={<span aria-hidden="true" />}
        >
          <button
            class="icon-button icon-button--ghost"
            type="button"
            title="Refresh connectors"
            onClick={() => void reloadSettings()}
          >
            <RefreshCw size={16} />
          </button>
        </Show>
      </header>

      <div class="settings-layout">
        <nav class="settings-nav" aria-label="Settings sections">
          <For each={NAV_GROUPS}>
            {(group) => (
              <div class="settings-nav__group">
                <span class="settings-nav__label">{group.label}</span>
                <For each={group.items}>
                  {(item) => (
                    <button
                      type="button"
                      classList={{
                        "settings-nav__item": true,
                        "settings-nav__item--active": tab() === item.id,
                      }}
                      onClick={() => setTab(item.id)}
                    >
                      <Dynamic component={item.icon} size={17} />
                      {item.label}
                    </button>
                  )}
                </For>
              </div>
            )}
          </For>
        </nav>

        <div class="settings-pane">
          <Switch>
            <Match when={tab() === "appearance"}>
              <AppearanceTab />
            </Match>
            <Match when={tab() === "chat"}>
              <ChatTab />
            </Match>
            <Match when={tab() === "connectors"}>
              <ConnectorsTab
                loading={isLoading()}
                providers={settings()?.providers ?? []}
                authorizingId={authorizingId()}
                onAuthorize={(id) => void authorize(id)}
                onCancelAuthorize={(id) => void cancelAuthorize(id)}
                onLogout={(id) => void logout(id)}
                onSelectModel={(id, modelId) => void selectModel(id, modelId)}
                onSetEnabled={(id, enabled) => void setEnabled(id, enabled)}
                onSaveSettings={(id, patch) => void saveAdapter(id, patch)}
              />
            </Match>
            <Match when={tab() === "personalization"}>
              <PersonalizationTab
                providers={settings()?.providers ?? []}
                onError={fail}
                onStatus={notify}
              />
            </Match>
            <Match when={tab() === "permissions"}>
              <PermissionsTab onError={fail} onStatus={notify} />
            </Match>
          </Switch>
        </div>
      </div>

      <Show when={error() || status()}>
        <Portal>
          <div class="settings-toasts">
            <Show when={error()}>
              <div class="settings-alert settings-alert--error" role="alert">
                {error()}
              </div>
            </Show>
            <Show when={status()}>
              <div class="settings-alert">{status()}</div>
            </Show>
          </div>
        </Portal>
      </Show>
    </main>
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}
