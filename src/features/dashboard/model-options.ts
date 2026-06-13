// Pure provider/model/reasoning option logic shared by the header selectors,
// the composer, and the workspace controller: which providers are selectable
// for chat, how custom model ids resolve to capability templates, what
// reasoning levels a model offers, and the provider status summary behind the
// header dot. No signals, no JSX.

import type {
  ConnectorProviderSummary,
  ConnectorSettingsSnapshot,
  LlmModel,
  ReasoningConfig,
  ReasoningOption,
} from "../../shared/api/mothership";
import type { ReasoningOptionId } from "./components/Composer";
import type {
  ProviderStatusSummary,
  ProviderStatusTone,
  SearchSelectOption,
} from "./types";

export function providerStatusBadgeLabel(status: ProviderStatusSummary) {
  const labels: Record<ProviderStatusTone, string> = {
    error: "Error",
    idle: "Setup",
    loading: "Loading",
    ready: "Ready",
    warmup: "Warmup",
  };

  return labels[status.tone];
}

/** Stable, JSON-encoded `[providerId, modelId]` option value. */
export function modelOptionValue(providerId: string, modelId: string) {
  return JSON.stringify([providerId, modelId]);
}

export function customModelOption(
  provider: ConnectorProviderSummary | undefined,
  models: LlmModel[],
  query: string,
): SearchSelectOption | undefined {
  if (!provider || !providerAcceptsCustomModelIds(provider)) {
    return undefined;
  }

  const modelId = normalizeCustomModelId(query);
  if (!modelId || models.some((model) => model.id === modelId)) {
    return undefined;
  }
  if (providerVisibilityModelIds(provider).has(modelId)) {
    return undefined;
  }

  return {
    detail: "Custom model id",
    label: `Use ${modelId}`,
    searchText: modelId,
    value: modelOptionValue(provider.id, modelId),
  };
}

export function withSelectedCustomModelOption(
  options: SearchSelectOption[],
  provider: ConnectorProviderSummary | undefined,
  selectedModelId: string | null | undefined,
) {
  if (
    !provider ||
    !providerAcceptsCustomModelIds(provider) ||
    !selectedModelId ||
    providerVisibilityModelIds(provider).has(selectedModelId) ||
    options.some((option) => option.value === modelOptionValue(provider.id, selectedModelId))
  ) {
    return options;
  }

  return [
    ...options,
    {
      detail: "Custom model id",
      label: selectedModelId,
      searchText: selectedModelId,
      value: modelOptionValue(provider.id, selectedModelId),
    },
  ];
}

export function normalizeCustomModelId(value: string) {
  const modelId = value.trim();
  if (
    modelId.length === 0 ||
    modelId.length > 256 ||
    /[\s\x00-\x1f\x7f]/.test(modelId)
  ) {
    return "";
  }
  return modelId;
}

export function parseModelOptionValue(value: string): [string, string] {
  try {
    const parsed = JSON.parse(value);
    return typeof parsed?.[0] === "string" && typeof parsed?.[1] === "string"
      ? [parsed[0], parsed[1]]
      : ["", ""];
  } catch {
    return ["", ""];
  }
}

function connectorProviderFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
) {
  if (!providerId) {
    return undefined;
  }
  return settings?.providers.find((provider) => provider.id === providerId);
}

export function selectableConnectorProviderFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
) {
  const provider = connectorProviderFor(settings, providerId);
  return provider && isProviderSelectableForChat(provider) ? provider : undefined;
}

export function selectableConnectorModelFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
  modelId: string | null | undefined,
) {
  const provider = selectableConnectorProviderFor(settings, providerId);
  if (!provider || !modelId) {
    return undefined;
  }

  return (
    provider.models.find((model) => model.id === modelId) ??
    customConnectorModelFor(provider, modelId)
  );
}

function customConnectorModelFor(
  provider: ConnectorProviderSummary,
  modelId: string,
): LlmModel | undefined {
  if (!providerAcceptsCustomModelIds(provider) || !normalizeCustomModelId(modelId)) {
    return undefined;
  }
  if (providerVisibilityModelIds(provider).has(modelId)) {
    return undefined;
  }

  const template = customModelCapabilityTemplate(provider, modelId);
  return {
    providerId: provider.id,
    providerLabel: provider.label,
    id: modelId,
    label: modelId,
    family: template?.family ?? "Custom",
    description: "Custom model id",
    capabilities: template?.capabilities ? [...template.capabilities] : ["text"],
    reasoning: template?.reasoning,
    fastMode: template?.fastMode,
    recommended: false,
  };
}

/** Best-effort capability template for a custom model id: the provider's known
 * model sharing the most significant name tokens (e.g. "opus", "4-8"). */
function customModelCapabilityTemplate(
  provider: ConnectorProviderSummary,
  modelId: string,
) {
  const targetTokens = significantModelTokens(modelId);
  if (targetTokens.size === 0) {
    return undefined;
  }

  let bestModel: LlmModel | undefined;
  let bestScore = 0;
  for (const model of provider.models) {
    const modelTokens = significantModelTokens(
      `${model.id} ${model.label} ${model.family}`,
    );
    let score = 0;
    for (const token of targetTokens) {
      if (modelTokens.has(token)) {
        score += 1;
      }
    }
    if (score > bestScore) {
      bestScore = score;
      bestModel = model;
    }
  }

  return bestScore > 0 ? bestModel : undefined;
}

function significantModelTokens(value: string) {
  const ignored = new Set([
    "claude",
    "model",
    "default",
    "recommended",
    "context",
    "with",
    "custom",
  ]);
  return new Set(
    value
      .toLowerCase()
      .split(/[^a-z0-9]+/)
      .map((token) => token.trim())
      .filter((token) => token.length >= 3 && !ignored.has(token)),
  );
}

export interface ReasoningSelectorOption {
  label: string;
  option: ReasoningOption;
  title: string;
  value: ReasoningOptionId;
}

export function reasoningSelectorOptions(model?: LlmModel): ReasoningSelectorOption[] {
  const reasoning = model?.reasoning;
  if (!reasoning?.supported) {
    return [];
  }

  return (reasoning.options ?? [])
    .filter((option) => option.id.trim().length > 0)
    .map((option) => {
      const label = reasoningOptionLabel(option);
      return {
        label,
        option,
        title: reasoningOptionTitle(option, label),
        value: option.id,
      };
    });
}

export function coerceReasoningOptionId(
  optionId: ReasoningOptionId | undefined,
  model?: LlmModel,
): ReasoningOptionId | undefined {
  const options = reasoningSelectorOptions(model);
  if (options.length === 0) {
    return undefined;
  }

  if (optionId && options.some((option) => option.value === optionId)) {
    return optionId;
  }

  return options.find((option) => option.option.recommended)?.value ?? options[0].value;
}

export function reasoningConfigForOption(
  optionId: ReasoningOptionId | undefined,
  model?: LlmModel,
): ReasoningConfig | null {
  const selected = reasoningSelectorOptions(model).find(
    (option) => option.value === optionId,
  );
  if (!selected || isReasoningConfigEmpty(selected.option.config)) {
    return null;
  }

  return selected.option.config;
}

export function modelSupportsFastMode(model?: LlmModel) {
  return Boolean(model?.fastMode?.supported);
}

export function fastModeLabel(model?: LlmModel) {
  return model?.fastMode?.label?.trim() || "Fast";
}

export function fastModeToggleTitle(model: LlmModel | undefined, enabled: boolean) {
  const label = fastModeLabel(model);
  const description = model?.fastMode?.description?.trim();
  const state = enabled ? "enabled" : "standard speed";
  return description ? `${label}: ${description}` : `${label} (${state})`;
}

function reasoningOptionLabel(option: ReasoningOption) {
  const id = option.id.trim().toLowerCase();
  const effort = option.config?.effort?.trim().toLowerCase();

  switch (id || effort) {
    case "auto":
      return "Авто";
    case "none":
      return "Отключено";
    case "minimal":
      return "Минимальный";
    case "low":
      return "Низкий";
    case "medium":
      return "Средний";
    case "high":
      return "Высокий";
    case "xhigh":
      return "Очень высокий";
    case "max":
      return "Максимальный";
    default:
      return option.label.trim() || option.id;
  }
}

function reasoningOptionTitle(option: ReasoningOption, label: string) {
  const description = option.description?.trim();
  if (description) {
    return description;
  }

  const id = option.id.trim().toLowerCase();
  if (id === "auto") {
    return "Дефолтный режим выбранной модели";
  }
  if (id === "none") {
    return "Не отправлять настройку рассуждения";
  }

  return `${label} уровень рассуждения`;
}

function isReasoningConfigEmpty(config?: ReasoningConfig | null) {
  return !config?.effort && !config?.budgetTokens && !config?.summary;
}

export function selectableProviderModelId(provider: ConnectorProviderSummary) {
  const selectedModelId = provider.selectedModelId;
  if (selectedModelId) {
    if (provider.models.some((model) => model.id === selectedModelId)) {
      return selectedModelId;
    }
    if (
      providerAcceptsCustomModelIds(provider) &&
      !providerVisibilityModelIds(provider).has(selectedModelId)
    ) {
      return selectedModelId;
    }
  }

  return provider.models[0]?.id;
}

export function isProviderSelectableForChat(provider: ConnectorProviderSummary) {
  if (provider.enabled === false) {
    return false;
  }

  if (requiresInteractiveAuth(provider)) {
    return false;
  }

  if (missingRequiredAdapterSettings(provider).length > 0) {
    return false;
  }

  return Boolean(selectableProviderModelId(provider));
}

function providerAcceptsCustomModelIds(provider: ConnectorProviderSummary | undefined) {
  return Boolean(provider?.settingsSchema.modelManagement.acceptsCustomModelIds);
}

function providerVisibilityModelIds(provider: ConnectorProviderSummary | undefined) {
  const fields = provider?.adapterSettings?.fields ?? [];
  return new Set(
    fields
      .filter((field) => field.kind === "model_visibility_list")
      .flatMap((field) => field.options.map((option) => option.value)),
  );
}

export function providerStatusSummary(
  provider: ConnectorProviderSummary | undefined,
): ProviderStatusSummary {
  if (!provider) {
    return {
      label: "No provider",
      tone: "idle",
      tooltip: "No model provider is selected.",
    };
  }

  const name = provider.label;
  if (provider.refreshStatus === "failed") {
    return {
      label: "Provider failed",
      tone: "error",
      tooltip: provider.modelError
        ? `${name}: ${provider.modelError}`
        : `${name}: provider refresh failed.`,
    };
  }

  if (provider.refreshStatus === "pending") {
    return {
      label: "Provider pending",
      tone: "loading",
      tooltip: `${name}: provider catalog has not loaded yet.`,
    };
  }

  if (provider.refreshStatus === "refreshing") {
    return {
      label: "Provider loading",
      tone: "loading",
      tooltip: `${name}: loading provider catalog and settings.`,
    };
  }

  if (requiresInteractiveAuth(provider)) {
    return {
      label: "Provider needs auth",
      tone: "warmup",
      tooltip: `${name}: authorization is required before chat.`,
    };
  }

  const missingSettings = missingRequiredAdapterSettings(provider);
  if (missingSettings.length > 0) {
    return {
      label: "Provider needs settings",
      tone: "warmup",
      tooltip: `${name}: required settings missing (${missingSettings.join(", ")}).`,
    };
  }

  if (provider.models.length === 0) {
    return {
      label: "Provider needs model",
      tone: "warmup",
      tooltip: `${name}: no chat model is available yet.`,
    };
  }

  if (!provider.runtimeReady) {
    return {
      label: "Provider needs warmup",
      tone: "warmup",
      tooltip: `${name}: catalog is ready, adapter will warm up on the next request.`,
    };
  }

  return {
    label: "Provider ready",
    tone: "ready",
    tooltip: `${name}: adapter is loaded and ready.`,
  };
}

function requiresInteractiveAuth(provider: ConnectorProviderSummary) {
  return (
    (provider.authKind === "oauth_internal" ||
      provider.authKind === "external_process") &&
    !provider.authenticated
  );
}

function missingRequiredAdapterSettings(provider: ConnectorProviderSummary) {
  const settings = provider.adapterSettings;
  if (!settings) {
    return [];
  }

  return settings.fields
    .filter((field) => field.required)
    .filter((field) => {
      if (field.kind === "secret") {
        return !settings.secrets[field.key]?.hasValue;
      }

      if (field.kind === "bool") {
        return false;
      }

      return !settings.values[field.key]?.trim();
    })
    .map((field) => field.label);
}
