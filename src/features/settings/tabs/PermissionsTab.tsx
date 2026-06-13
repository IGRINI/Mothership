import { createSignal, For, onMount, Show } from "solid-js";
import { Plus, X } from "lucide-solid";

import type { ToolPolicySettings } from "../../../shared/api/mothership";
import {
  getChangeJournalRetention,
  getToolPolicy,
  setChangeJournalRetention,
  setToolPolicy,
} from "../../../shared/api/mothership";

const TOOL_CATALOG: { name: string; label: string; description: string }[] = [
  {
    name: "run_command",
    label: "Run commands",
    description: "Execute local shell / OS commands.",
  },
  {
    name: "read_file",
    label: "Read files",
    description: "Read a workspace file (read-only).",
  },
  {
    name: "write_file",
    label: "Write files",
    description: "Create or overwrite a workspace file.",
  },
  {
    name: "edit_file",
    label: "Edit files",
    description: "Content-addressed string replacement in a file.",
  },
  {
    name: "apply_patch",
    label: "Apply patches",
    description: "Multi-file patch applied all-or-nothing.",
  },
  {
    name: "search_text",
    label: "Search text",
    description: "Content search across the workspace (read-only).",
  },
];

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
}) {
  const [policy, setPolicy] = createSignal<ToolPolicySettings>(EMPTY_POLICY);
  const [loading, setLoading] = createSignal(true);
  const [retention, setRetention] = createSignal(10);

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
            ? "Change journal: keeping everything."
            : `Change journal: keeping changes from the last ${stored} messages per project.`,
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
      props.onStatus("Permission rules saved.");
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
        <h2>Permissions</h2>
        <p>
          Control how the agent uses tools: an allowlist / denylist for shell
          commands, and which tools are available at all. The approval mode
          (manual / auto / yolo) is per-chat — set it under the chat's input.
        </p>
      </div>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Command allowlist</h3>
          <p>
            Programs here are auto-approved — they skip the approval prompt
            regardless of mode. Match is by name (e.g. <code>npm</code>,{" "}
            <code>git</code>).
          </p>
        </div>
        <CommandRules
          tone="allow"
          values={policy().commandAllow}
          placeholder="e.g. npm"
          onAdd={(value) => addCommand("commandAllow", value)}
          onRemove={(value) => removeCommand("commandAllow", value)}
        />
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Command denylist</h3>
          <p>
            Programs here are always blocked, even in Yolo mode. Deny wins over
            allow if a program is in both.
          </p>
        </div>
        <CommandRules
          tone="deny"
          values={policy().commandDeny}
          placeholder="e.g. rm"
          onAdd={(value) => addCommand("commandDeny", value)}
          onRemove={(value) => removeCommand("commandDeny", value)}
        />
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Change journal</h3>
          <p>
            Every file change an agent makes is snapshotted so it can be
            reviewed and reverted. History is kept for the last N agent{" "}
            <em>messages</em> per project — one message may carry hundreds of
            edits and they're kept (or pruned) together. <code>0</code> keeps
            everything.
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
                ? "Unlimited history"
                : `Changes from the last ${retention()} messages per project`}
            </span>
          </div>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Tool access</h3>
          <p>Turn individual tools off so the agent can never call them.</p>
        </div>
        <div class="tool-list">
          <For each={TOOL_CATALOG}>
            {(tool) => {
              const enabled = () => !policy().disabledTools.includes(tool.name);
              return (
                <div class="tool-row">
                  <div class="tool-row__meta">
                    <span class="tool-row__name">
                      {tool.label}
                      <code>{tool.name}</code>
                    </span>
                    <span class="tool-row__desc">{tool.description}</span>
                  </div>
                  <label class="switch" title={enabled() ? "Enabled" : "Disabled"}>
                    <input
                      type="checkbox"
                      checked={enabled()}
                      disabled={loading()}
                      onChange={(event) =>
                        toggleTool(tool.name, event.currentTarget.checked)
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
          Add
        </button>
      </div>
      <Show
        when={props.values.length > 0}
        fallback={<span class="rule-empty">Nothing here yet.</span>}
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
                  aria-label={`Remove ${value}`}
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
