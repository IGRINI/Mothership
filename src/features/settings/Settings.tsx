import {
  createEffect,
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
  Route,
  Search,
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
  setFeatureRoute,
  setProviderEnabled,
  setSelectedModel,
} from "../../shared/api/mothership";
import { startWindowDrag } from "../../shared/window-drag";
import { AppearanceTab } from "./tabs/AppearanceTab";
import { ChatTab } from "./tabs/ChatTab";
import { ConnectorsTab } from "./tabs/ConnectorsTab";
import { PermissionsTab } from "./tabs/PermissionsTab";
import { PersonalizationTab } from "./tabs/PersonalizationTab";
import { ServicesTab } from "./tabs/ServicesTab";
import { settingsCopy, type SettingsTabId } from "./settings-copy";
import {
  EMPTY_SETTINGS_SEARCH,
  searchSettings,
  SettingsHighlight,
  tabMatches,
  type SettingsSearchResult,
} from "./settings-search";

interface TabDef {
  id: SettingsTabId;
  icon: Component<LucideProps>;
}

const NAV_GROUPS: { id: string; items: TabDef[] }[] = [
  {
    id: "general",
    items: [
      { id: "appearance", icon: Palette },
      { id: "chat", icon: MessageSquare },
    ],
  },
  {
    id: "providers",
    items: [
      { id: "connectors", icon: Plug },
      { id: "services", icon: Route },
      { id: "personalization", icon: Sparkles },
    ],
  },
  {
    id: "tools",
    items: [{ id: "permissions", icon: ShieldCheck }],
  },
];

export function Settings(props: { onBack: () => void }) {
  const [tab, setTab] = createSignal<SettingsTabId>("appearance");
  const [settings, setSettings] = createSignal<ConnectorSettingsSnapshot>();
  const [error, setError] = createSignal("");
  const [status, setStatus] = createSignal("");
  const [isLoading, setIsLoading] = createSignal(true);
  const [authorizingId, setAuthorizingId] = createSignal<string>();
  const [query, setQuery] = createSignal("");
  const [paletteOpen, setPaletteOpen] = createSignal(false);
  let searchInput: HTMLInputElement | undefined;
  let unlistenConnectorSettings: (() => void) | undefined;
  let statusTimer: number | undefined;

  const copy = () => settingsCopy();
  const searchState = createMemo(() =>
    query().trim()
      ? searchSettings(query(), copy().search.documents)
      : EMPTY_SETTINGS_SEARCH,
  );
  const tabLabel = createMemo(() => copy().nav.tabs[tab()] ?? "");
  const initialConnectorLoading = () => isLoading() && !settings();

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

  createEffect(() => {
    const search = searchState();
    if (!search.active || tabMatches(search, tab())) {
      return;
    }
    const nextTab = search.results[0]?.tabId;
    if (nextTab) {
      setTab(nextTab);
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

  function handleKeyDown(event: KeyboardEvent) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      setPaletteOpen(true);
      searchInput?.focus();
      searchInput?.select();
    }
  }

  onMount(() => {
    window.addEventListener("keydown", handleKeyDown);
    onCleanup(() => window.removeEventListener("keydown", handleKeyDown));
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

  function openSearchResult(result: SettingsSearchResult) {
    setTab(result.tabId);
    setPaletteOpen(false);
    window.requestAnimationFrame(() => {
      const section = document.querySelector<HTMLElement>(
        `[data-settings-section="${result.id}"]`,
      );
      section?.scrollIntoView({ block: "start", behavior: "smooth" });
      section?.classList.add("settings-card--flash");
      window.setTimeout(() => section?.classList.remove("settings-card--flash"), 900);
    });
  }

  async function reloadSettings() {
    const initialLoad = !settings();
    if (initialLoad) {
      setIsLoading(true);
    }
    setError("");

    try {
      setSettings(await getConnectorSettings());
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    } finally {
      setIsLoading(false);
    }
  }

  // Reflect a preference change in the UI on the NEXT FRAME, then reconcile with
  // the server: apply the edit to the local snapshot immediately, fire the async
  // call in the background, and roll back only if it rejects. So switches and
  // selectors never wait on the IPC round-trip to visually respond.
  function applyOptimistic(
    edit: (snapshot: ConnectorSettingsSnapshot) => ConnectorSettingsSnapshot,
    commit: () => Promise<ConnectorSettingsSnapshot>,
    done?: () => void,
  ) {
    setError("");
    const previous = settings();
    if (previous) {
      setSettings(edit(previous));
    }
    void commit()
      .then((snapshot) => {
        setSettings(snapshot);
        done?.();
      })
      .catch((caughtError) => {
        if (previous) {
          setSettings(previous);
        }
        fail(errorMessage(caughtError));
      });
  }

  function selectModel(providerId: string, modelId: string) {
    applyOptimistic(
      (snapshot) => ({
        ...snapshot,
        selectedModel: { ...snapshot.selectedModel, providerId, modelId },
        providers: snapshot.providers.map((provider) => ({
          ...provider,
          selectedModelId: provider.id === providerId ? modelId : null,
        })),
      }),
      () => setSelectedModel(providerId, modelId),
      () => notify(copy().shell.modelSelectionSaved),
    );
  }

  function selectFeatureRoute(
    feature: string,
    providerId: string,
    modelId: string,
  ) {
    applyOptimistic(
      (snapshot) => {
        const existing = snapshot.featureRoutes.find(
          (route) => route.feature === feature,
        );
        return {
          ...snapshot,
          featureRoutes: [
            ...snapshot.featureRoutes.filter(
              (route) => route.feature !== feature,
            ),
            {
              feature,
              providerId,
              modelId,
              options: existing?.options ?? {},
              updatedAt: existing?.updatedAt ?? "",
            },
          ],
        };
      },
      () => setFeatureRoute(feature, providerId, modelId),
      () => notify(copy().shell.serviceSelectionSaved),
    );
  }

  // In-flight save marker: the form's values are already local state, so the
  // next-frame cue here is the Save button disabling until the IPC settles.
  const [savingAdapterId, setSavingAdapterId] = createSignal<string>();

  async function saveAdapter(
    providerId: string,
    patch: Record<string, AdapterSettingPatchValue>,
  ) {
    if (savingAdapterId()) {
      return;
    }
    setError("");
    setSavingAdapterId(providerId);
    try {
      setSettings(await saveAdapterSettings(providerId, patch));
      notify(copy().shell.adapterSettingsSaved);
    } catch (caughtError) {
      fail(errorMessage(caughtError));
    } finally {
      setSavingAdapterId(undefined);
    }
  }

  function setEnabled(providerId: string, enabled: boolean) {
    applyOptimistic(
      (snapshot) => ({
        ...snapshot,
        providers: snapshot.providers.map((provider) =>
          provider.id === providerId ? { ...provider, enabled } : provider,
        ),
      }),
      () => setProviderEnabled(providerId, enabled),
      () =>
        notify(
          enabled ? copy().shell.connectorEnabled : copy().shell.connectorDisabled,
        ),
    );
  }

  async function authorize(providerId: string) {
    if (authorizingId()) {
      return;
    }
    setError("");
    notify(copy().shell.authorizing);
    setAuthorizingId(providerId);

    try {
      const snapshot = await authenticateAdapter(providerId);
      setSettings(snapshot);
      const provider = snapshot.providers.find((item) => item.id === providerId);
      notify(
        provider?.authenticated
          ? copy().shell.authorized
          : copy().shell.authorizationCancelled,
      );
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
      notify(copy().shell.loggedOut);
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
          {copy().shell.back}
        </button>
        <div>
          <h1>{copy().shell.title}</h1>
          <span>{tabLabel()}</span>
        </div>
        <div class="settings-search" onMouseDown={(event) => event.stopPropagation()}>
          <Search size={15} />
          <input
            ref={searchInput}
            type="search"
            value={query()}
            placeholder={copy().shell.searchPlaceholder}
            spellcheck={false}
            onFocus={() => setPaletteOpen(true)}
            onInput={(event) => {
              setQuery(event.currentTarget.value);
              setPaletteOpen(true);
            }}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                setPaletteOpen(false);
                searchInput?.blur();
              }
              if (event.key === "Enter") {
                const first = searchState().results[0];
                if (first) {
                  event.preventDefault();
                  openSearchResult(first);
                }
              }
            }}
          />
          <kbd>{copy().shell.searchShortcut}</kbd>
          <Show when={paletteOpen()}>
            <SettingsCommandPalette
              query={query()}
              results={searchState().results}
              emptyText={copy().search.empty}
              hintText={copy().search.hint}
              noMatchesText={copy().search.noMatches}
              openHint={copy().search.openHint}
              search={searchState()}
              tabLabel={(id) => copy().nav.tabs[id]}
              onOpen={openSearchResult}
            />
          </Show>
        </div>
        <Show
          when={tab() === "connectors" || tab() === "services"}
          fallback={<span aria-hidden="true" />}
        >
          <button
            class="icon-button icon-button--ghost"
            type="button"
            title={copy().shell.refreshConnectors}
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
                <span class="settings-nav__label">
                  {copy().nav.groups.find((item) => item.id === group.id)?.label ??
                    group.id}
                </span>
                <For each={group.items}>
                  {(item) => (
                    <button
                      type="button"
                      disabled={!tabMatches(searchState(), item.id)}
                      classList={{
                        "settings-nav__item": true,
                        "settings-nav__item--active": tab() === item.id,
                        "settings-nav__item--dimmed": !tabMatches(
                          searchState(),
                          item.id,
                        ),
                      }}
                      onClick={() => setTab(item.id)}
                    >
                      <Dynamic component={item.icon} size={17} />
                      <SettingsHighlight
                        text={copy().nav.tabs[item.id]}
                        search={searchState()}
                      />
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
              <AppearanceTab search={searchState()} />
            </Match>
            <Match when={tab() === "chat"}>
              <ChatTab search={searchState()} />
            </Match>
            <Match when={tab() === "connectors"}>
              <ConnectorsTab
                loading={initialConnectorLoading()}
                providers={settings()?.providers ?? []}
                authorizingId={authorizingId()}
                onAuthorize={(id) => void authorize(id)}
                onCancelAuthorize={(id) => void cancelAuthorize(id)}
                onLogout={(id) => void logout(id)}
                onSelectModel={(id, modelId) => void selectModel(id, modelId)}
                onSetEnabled={(id, enabled) => void setEnabled(id, enabled)}
                onSaveSettings={(id, patch) => void saveAdapter(id, patch)}
                savingId={savingAdapterId()}
                search={searchState()}
              />
            </Match>
            <Match when={tab() === "services"}>
              <ServicesTab
                loading={initialConnectorLoading()}
                providers={settings()?.providers ?? []}
                featureRoutes={settings()?.featureRoutes ?? []}
                onSelectFeatureRoute={(feature, id, modelId) =>
                  void selectFeatureRoute(feature, id, modelId)
                }
                search={searchState()}
              />
            </Match>
            <Match when={tab() === "personalization"}>
              <PersonalizationTab
                providers={settings()?.providers ?? []}
                onError={fail}
                onStatus={notify}
                search={searchState()}
              />
            </Match>
            <Match when={tab() === "permissions"}>
              <PermissionsTab
                onError={fail}
                onStatus={notify}
                search={searchState()}
              />
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

function SettingsCommandPalette(props: {
  query: string;
  results: SettingsSearchResult[];
  emptyText: string;
  hintText: string;
  noMatchesText: string;
  openHint: string;
  search: ReturnType<typeof searchSettings>;
  tabLabel: (id: SettingsTabId) => string;
  onOpen: (result: SettingsSearchResult) => void;
}) {
  return (
    <div class="settings-command-palette">
      <Show
        when={props.query.trim().length > 0}
        fallback={
          <div class="settings-command-palette__empty">
            <span>{props.emptyText}</span>
            <small>{props.hintText}</small>
          </div>
        }
      >
        <Show
          when={props.results.length > 0}
          fallback={
            <div class="settings-command-palette__empty">
              <span>{props.noMatchesText}</span>
              <small>{props.hintText}</small>
            </div>
          }
        >
          <div class="settings-command-palette__list">
            <For each={props.results.slice(0, 9)}>
              {(result) => (
                <button
                  class="settings-command-palette__item"
                  type="button"
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => props.onOpen(result)}
                >
                  <span>
                    <strong>
                      <SettingsHighlight text={result.title} search={props.search} />
                    </strong>
                    <small>{props.tabLabel(result.tabId)}</small>
                  </span>
                  <span class="settings-command-palette__desc">
                    <SettingsHighlight
                      text={result.description}
                      search={props.search}
                    />
                  </span>
                  <kbd>{props.openHint}</kbd>
                </button>
              )}
            </For>
          </div>
        </Show>
      </Show>
    </div>
  );
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}
