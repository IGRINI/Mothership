// Header / composer model controls: the model + provider searchable selectors,
// the provider status dot, the fast-mode toggle, and the tool approval-mode
// menu. All derive from the connector snapshot + the open chat's model.

import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Check, ChevronDown, ShieldCheck, Zap } from "lucide-solid";

import type {
  ConnectorProviderSummary,
  ConnectorSettingsSnapshot,
  LlmModel,
  ToolApprovalMode,
} from "../../../shared/api/mothership";
import {
  customModelOption,
  fastModeLabel,
  fastModeToggleTitle,
  isProviderSelectableForChat,
  modelOptionValue,
  modelSupportsFastMode,
  parseModelOptionValue,
  providerStatusBadgeLabel,
  providerStatusSummary,
  selectableConnectorProviderFor,
  selectableProviderModelId,
  withSelectedCustomModelOption,
} from "../model-options";
import type { SearchSelectOption } from "../types";
import { SearchSelect } from "./SearchSelect";

const TOOL_APPROVAL_MODE_OPTIONS: Array<{
  mode: ToolApprovalMode;
  label: string;
  title: string;
}> = [
  {
    mode: "manual",
    label: "Manual",
    title: "Ask before mutating tools and non-read-only commands.",
  },
  {
    mode: "auto_safe",
    label: "Auto",
    title: "Auto-run ordinary actions; ask before destructive commands or escalation.",
  },
  {
    mode: "yolo",
    label: "YOLO",
    title: "Run without approval prompts and approve currently pending prompts.",
  },
];

// Menu-select for the tool approval mode (Manual / Auto / YOLO), styled to match
// the reasoning selector — they sit side by side under the composer input. The
// trigger is tinted by mode (green for auto, orange for yolo) to keep the
// at-a-glance signal the old segmented control had.
export function ApprovalModeMenu(props: {
  mode: ToolApprovalMode;
  disabled?: boolean;
  onChange: (mode: ToolApprovalMode) => void;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  let rootRef: HTMLDivElement | undefined;
  const selected = () =>
    TOOL_APPROVAL_MODE_OPTIONS.find((option) => option.mode === props.mode) ??
    TOOL_APPROVAL_MODE_OPTIONS[0];

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }
      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        setIsOpen(false);
      }
    };
    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() =>
      document.removeEventListener("pointerdown", handlePointerDown),
    );
  });

  const choose = (mode: ToolApprovalMode) => {
    props.onChange(mode);
    setIsOpen(false);
  };

  return (
    <div
      ref={rootRef}
      classList={{
        "reasoning-selector": true,
        "approval-menu": true,
        "approval-menu--auto_safe": props.mode === "auto_safe",
        "approval-menu--yolo": props.mode === "yolo",
      }}
    >
      <button
        class="reasoning-selector__trigger"
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="menu"
        disabled={props.disabled}
        title={selected().title}
        onClick={() => {
          if (!props.disabled) {
            setIsOpen((value) => !value);
          }
        }}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            setIsOpen(false);
          }
        }}
      >
        <ShieldCheck size={15} />
        <span>{selected().label}</span>
        <ChevronDown
          classList={{
            "reasoning-selector__chevron": true,
            "reasoning-selector__chevron--open": isOpen(),
          }}
          size={13}
        />
      </button>

      <Show when={isOpen()}>
        <div class="reasoning-selector__popover" role="menu">
          <div class="reasoning-selector__heading">Approval mode</div>
          <For each={TOOL_APPROVAL_MODE_OPTIONS}>
            {(option) => (
              <button
                classList={{
                  "reasoning-selector__item": true,
                  "reasoning-selector__item--selected": option.mode === props.mode,
                }}
                type="button"
                role="menuitemradio"
                aria-checked={option.mode === props.mode}
                title={option.title}
                onClick={() => choose(option.mode)}
              >
                <span>{option.label}</span>
                <Show when={option.mode === props.mode}>
                  <Check size={14} />
                </Show>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

export function FastModeToggle(props: {
  disabled: boolean;
  enabled: boolean;
  model?: LlmModel;
  onChange: (enabled: boolean) => void;
}) {
  return (
    <Show when={modelSupportsFastMode(props.model)}>
      <button
        classList={{
          "fast-mode-toggle": true,
          "fast-mode-toggle--active": props.enabled,
        }}
        type="button"
        role="switch"
        aria-checked={props.enabled}
        disabled={props.disabled}
        title={fastModeToggleTitle(props.model, props.enabled)}
        onClick={() => props.onChange(!props.enabled)}
      >
        <Zap size={14} />
        <span>{fastModeLabel(props.model)}</span>
      </button>
    </Show>
  );
}

export function ModelSelector(props: {
  onSelectModel: (providerId: string, modelId: string) => void;
  selected?: { providerId?: string | null; modelId?: string | null };
  settings?: ConnectorSettingsSnapshot;
}) {
  const activeProvider = () =>
    selectableConnectorProviderFor(
      props.settings,
      props.selected?.providerId ?? props.settings?.selectedModel.providerId,
    );
  const models = () => activeProvider()?.models ?? [];
  const isRefreshing = () => {
    const provider = activeProvider();
    const providers = props.settings?.providers;

    if (!provider) {
      return providers?.some(
        (item) =>
          item.refreshStatus === "pending" ||
          item.refreshStatus === "refreshing",
      ) ?? true;
    }

    return (
      provider.refreshStatus === "pending" ||
      provider.refreshStatus === "refreshing"
    );
  };
  const hasConnectorError = () => {
    const provider = activeProvider();
    if (provider) {
      return Boolean(provider.modelError && provider.models.length === 0);
    }

    return (
      props.settings?.providers.some(
        (item) => item.modelError && item.models.length === 0,
      ) ?? false
    );
  };
  const selectedValue = () => {
    const providerId =
      props.selected?.providerId ?? props.settings?.selectedModel.providerId;
    const modelId =
      props.selected?.modelId ?? props.settings?.selectedModel.modelId;
    return providerId && modelId
      ? modelOptionValue(providerId, modelId)
      : "";
  };
  const placeholder = () =>
    isRefreshing()
      ? "Loading models..."
      : hasConnectorError()
        ? "Connector unavailable"
        : activeProvider()
          ? "No models for provider"
          : "No models connected";
  const options = createMemo<SearchSelectOption[]>(() =>
    withSelectedCustomModelOption(
      models().map((model) => ({
        detail: model.id === model.label ? model.providerLabel : model.id,
        label: model.label,
        searchText: `${model.providerLabel} ${model.label} ${model.id}`,
        value: modelOptionValue(model.providerId, model.id),
      })),
      activeProvider(),
      props.selected?.modelId ?? props.settings?.selectedModel.modelId,
    ),
  );

  return (
    <SearchSelect
      ariaLabel="Active model"
      class="model-search-select"
      emptyLabel={placeholder()}
      options={options()}
      placeholder={placeholder()}
      value={selectedValue()}
      createOption={(query) => customModelOption(activeProvider(), models(), query)}
      onSelect={(value) => {
        const [providerId, modelId] = parseModelOptionValue(value);
        if (providerId && modelId) {
          props.onSelectModel(providerId, modelId);
        }
      }}
    />
  );
}

export function ProviderStatusDot(props: { provider?: ConnectorProviderSummary }) {
  const status = () => providerStatusSummary(props.provider);

  return (
    <span
      class={`provider-status-dot provider-status-dot--${status().tone}`}
      title={status().tooltip}
      aria-label={status().label}
    />
  );
}

export function ProviderSelector(props: {
  onSelectModel: (providerId: string, modelId: string) => void;
  selected?: { providerId?: string | null; modelId?: string | null };
  settings?: ConnectorSettingsSnapshot;
}) {
  const providers = () =>
    (props.settings?.providers ?? []).filter(isProviderSelectableForChat);
  const currentProviderId = () =>
    props.selected?.providerId ?? props.settings?.selectedModel.providerId;
  const selectedProviderId = () => currentProviderId() ?? "";
  const activeProvider = () =>
    providers().find((provider) => provider.id === currentProviderId());
  const status = () => providerStatusSummary(activeProvider());
  const options = createMemo<SearchSelectOption[]>(() =>
    providers().map((provider) => {
      const providerStatus = providerStatusSummary(provider);
      const modelId = selectableProviderModelId(provider);
      const modelCount = provider.models.length;

      return {
        detail: modelId
          ? `${modelCount} ${modelCount === 1 ? "model" : "models"}`
          : providerStatus.tooltip,
        disabled: !modelId,
        label: provider.label,
        searchText: [
          provider.label,
          providerStatus.label,
          providerStatus.tooltip,
          ...provider.models.map((model) => `${model.label} ${model.id}`),
        ].join(" "),
        status: {
          label: providerStatusBadgeLabel(providerStatus),
          tone: providerStatus.tone,
        },
        title: providerStatus.tooltip,
        value: provider.id,
      };
    }),
  );

  return (
    <SearchSelect
      ariaLabel="Active provider"
      class="provider-search-select"
      emptyLabel="No providers"
      options={options()}
      placeholder="No provider"
      title={status().tooltip}
      value={selectedProviderId()}
      onSelect={(providerId) => {
        const provider = providers().find((item) => item.id === providerId);
        const modelId = provider ? selectableProviderModelId(provider) : undefined;
        if (provider && modelId) {
          props.onSelectModel(provider.id, modelId);
        }
      }}
    />
  );
}
