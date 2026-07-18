import { createSignal, For, onMount, Show } from "solid-js";
import { Plus, X } from "lucide-solid";

import type { ToolPolicySettings } from "../../../shared/api/mothership";
import {
  getChangeJournalRetention,
  getToolPolicy,
  setChangeJournalRetention,
  setToolPolicy,
} from "../../../shared/api/mothership";
import { settingsCopy } from "../settings-copy";
import {
  sectionMatches,
  SettingsHighlight,
  type SettingsSearchState,
} from "../settings-search";

const TOOL_CATALOG = [
  "run_command",
  "read_file",
  "write_file",
  "edit_file",
  "apply_patch",
  "search_text",
] as const;

const EMPTY_POLICY: ToolPolicySettings = {
  commandAllow: [],
  commandDeny: [],
  disabledTools: [],
};

/**
 * Permissions tab: the global approval mode plus the user's command allow/deny
 * lists and per-tool access toggles. Each edit round-trips to Core (which
 * sanitizes and applies the rules live), and we adopt the canonical result it
 * returns so the UI always mirrors what the runtime now enforces.
 */
export function PermissionsTab(props: {
  onError: (message: string) => void;
  onStatus: (message: string) => void;
  search: SettingsSearchState;
}) {
  const [policy, setPolicy] = createSignal<ToolPolicySettings>(EMPTY_POLICY);
  const [loading, setLoading] = createSignal(true);
  const [retention, setRetention] = createSignal(10);
  const copy = () => settingsCopy().permissions;

  onMount(async () => {
    try {
      setPolicy(await getToolPolicy());
      setRetention(await getChangeJournalRetention());
    } catch (error) {
      props.onError(errorMessage(error));
    } finally {
      setLoading(false);
    }
  });

  // Optimistic: the field reflects the new value immediately; reconcile with
  // the stored canonical value, roll back if Core rejects it.
  function commitRetention(raw: number) {
    const value = Number.isFinite(raw) ? Math.max(0, Math.floor(raw)) : 0;
    const previous = retention();
    if (value === previous) {
      return;
    }
    setRetention(value);
    setChangeJournalRetention(value)
      .then((stored) => {
        setRetention(stored);
        props.onStatus(
          stored === 0
            ? copy().keepEverything
            : copy().keepLast(stored),
        );
      })
      .catch((error: unknown) => {
        setRetention(previous);
        props.onError(errorMessage(error));
      });
  }

  async function commit(next: ToolPolicySettings) {
    const previous = policy();
    setPolicy(next);
    try {
      setPolicy(await setToolPolicy(next));
      props.onStatus(copy().rulesSaved);
    } catch (error) {
      setPolicy(previous);
      props.onError(errorMessage(error));
    }
  }

  const addCommand = (list: "commandAllow" | "commandDeny", raw: string) => {
    const value = raw.trim().toLowerCase();
    if (!value) {
      return;
    }
    const current = policy();
    if (current[list].includes(value)) {
      return;
    }
    void commit({ ...current, [list]: [...current[list], value] });
  };

  const removeCommand = (list: "commandAllow" | "commandDeny", value: string) =>
    void commit({
      ...policy(),
      [list]: policy()[list].filter((item) => item !== value),
    });

  const toggleTool = (name: string, enabled: boolean) => {
    const current = policy();
    const disabled = new Set(current.disabledTools);
    if (enabled) {
      disabled.delete(name);
    } else {
      disabled.add(name);
    }
    void commit({ ...current, disabledTools: [...disabled] });
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
            "permissions.allowlist",
          ),
        }}
        data-settings-section="permissions.allowlist"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().allowTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().allowDescription}
              search={props.search}
            />
          </p>
        </div>
        <CommandRules
          tone="allow"
          values={policy().commandAllow}
          placeholder={copy().allowPlaceholder}
          onAdd={(value) => addCommand("commandAllow", value)}
          onRemove={(value) => removeCommand("commandAllow", value)}
        />
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "permissions.denylist",
          ),
        }}
        data-settings-section="permissions.denylist"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().denyTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().denyDescription}
              search={props.search}
            />
          </p>
        </div>
        <CommandRules
          tone="deny"
          values={policy().commandDeny}
          placeholder={copy().denyPlaceholder}
          onAdd={(value) => addCommand("commandDeny", value)}
          onRemove={(value) => removeCommand("commandDeny", value)}
        />
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "permissions.change-journal",
          ),
        }}
        data-settings-section="permissions.change-journal"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().changeJournalTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().changeJournalDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="rule-editor">
          <div class="rule-input-row">
            <input
              type="number"
              min="0"
              step="1"
              value={retention()}
              disabled={loading()}
              onChange={(event) =>
                commitRetention(event.currentTarget.valueAsNumber)
              }
            />
            <span class="rule-empty">
              {retention() === 0
                ? copy().unlimitedHistory
                : copy().changesFromLast(retention())}
            </span>
          </div>
        </div>
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "permissions.tool-access",
          ),
        }}
        data-settings-section="permissions.tool-access"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().toolAccessTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().toolAccessDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="tool-list">
          <For each={TOOL_CATALOG}>
            {(tool) => {
              const enabled = () => !policy().disabledTools.includes(tool);
              const info = () => copy().toolCatalog[tool];
              return (
                <div class="tool-row">
                  <div class="tool-row__meta">
                    <span class="tool-row__name">
                      {info().label}
                      <code>{tool}</code>
                    </span>
                    <span class="tool-row__desc">{info().description}</span>
                  </div>
                  <label
                    class="switch"
                    title={
                      enabled()
                        ? settingsCopy().common.enabled
                        : settingsCopy().common.disabled
                    }
                  >
                    <input
                      type="checkbox"
                      checked={enabled()}
                      disabled={loading()}
                      onChange={(event) =>
                        toggleTool(tool, event.currentTarget.checked)
                      }
                    />
                    <span class="switch__track" />
                    <span class="switch__thumb" />
                  </label>
                </div>
              );
            }}
          </For>
        </div>
      </section>
    </div>
  );
}

function CommandRules(props: {
  tone: "allow" | "deny";
  values: string[];
  placeholder: string;
  onAdd: (value: string) => void;
  onRemove: (value: string) => void;
}) {
  const [draft, setDraft] = createSignal("");

  const submit = () => {
    props.onAdd(draft());
    setDraft("");
  };

  return (
    <div class="rule-editor">
      <div class="rule-input-row">
        <input
          type="text"
          value={draft()}
          placeholder={props.placeholder}
          spellcheck={false}
          autocapitalize="none"
          onInput={(event) => setDraft(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              submit();
            }
          }}
        />
        <button
          class="settings-secondary-button"
          type="button"
          disabled={!draft().trim()}
          onClick={submit}
        >
          <Plus size={15} />
          {settingsCopy().common.add}
        </button>
      </div>
      <Show
        when={props.values.length > 0}
        fallback={<span class="rule-empty">{settingsCopy().common.noneYet}</span>}
      >
        <div class="rule-list">
          <For each={props.values}>
            {(value) => (
              <span
                classList={{
                  "rule-chip": true,
                  [`rule-chip--${props.tone}`]: true,
                }}
              >
                {value}
                <button
                  class="rule-chip__remove"
                  type="button"
                  aria-label={`${settingsCopy().common.remove} ${value}`}
                  onClick={() => props.onRemove(value)}
                >
                  <X size={12} />
                </button>
              </span>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
