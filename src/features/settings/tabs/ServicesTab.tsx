import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import type {
  ConnectorProviderSummary,
  FeatureRoute,
} from "../../../shared/api/mothership";

type ServiceRouteOption = {
  providerId: string;
  providerLabel: string;
  modelId: string;
  modelLabel: string;
  recommended: boolean;
};

type ServiceRouteGroup = {
  feature: string;
  label: string;
  options: ServiceRouteOption[];
};

const SERVICE_ROUTE_VALUE_SEPARATOR = "\u001f";

export function ServicesTab(props: {
  loading: boolean;
  providers: ConnectorProviderSummary[];
  featureRoutes: FeatureRoute[];
  onSelectFeatureRoute: (
    feature: string,
    providerId: string,
    modelId: string,
  ) => void;
}) {
  const currentServiceGroups = createMemo<ServiceRouteGroup[]>(() => {
    const groups = new Map<string, ServiceRouteGroup>();

    for (const provider of props.providers) {
      if (!providerCanServeServices(provider)) {
        continue;
      }

      for (const service of provider.services) {
        if (service.models.length === 0) {
          continue;
        }

        let group = groups.get(service.feature);
        if (!group) {
          group = {
            feature: service.feature,
            label: serviceLabel(service.feature, service.label),
            options: [],
          };
          groups.set(service.feature, group);
        }

        for (const model of service.models) {
          group.options.push({
            providerId: provider.id,
            providerLabel: provider.label,
            modelId: model.id,
            modelLabel: model.label,
            recommended: model.recommended ?? false,
          });
        }
      }
    }

    return [...groups.values()]
      .map((group) => ({
        ...group,
        options: group.options.sort(compareServiceRouteOptions),
      }))
      .sort(compareServiceRouteGroups);
  });
  const [stableServiceGroups, setStableServiceGroups] = createSignal<
    ServiceRouteGroup[]
  >([]);

  const hasTransientProviderRefresh = () =>
    props.providers.some(
      (provider) =>
        provider.refreshStatus === "pending" ||
        provider.refreshStatus === "refreshing",
    );

  createEffect(() => {
    const groups = currentServiceGroups();
    if (groups.length > 0 || !hasTransientProviderRefresh()) {
      setStableServiceGroups(groups);
    }
  });

  const serviceGroups = () => {
    const groups = currentServiceGroups();
    if (groups.length > 0 || !hasTransientProviderRefresh()) {
      return groups;
    }
    return stableServiceGroups();
  };

  const selectedRoute = (feature: string) =>
    props.featureRoutes.find((route) => route.feature === feature);

  const selectedValue = (group: ServiceRouteGroup) => {
    const route = selectedRoute(group.feature);
    if (!route) {
      return "";
    }

    const hasSelectedOption = group.options.some(
      (option) =>
        option.providerId === route.providerId &&
        option.modelId === route.modelId,
    );
    return hasSelectedOption
      ? serviceRouteValue(route.providerId, route.modelId)
      : "";
  };

  const selectedHint = (group: ServiceRouteGroup) => {
    const route = selectedRoute(group.feature);
    if (!route) {
      return "No route selected";
    }

    const option = group.options.find(
      (item) =>
        item.providerId === route.providerId && item.modelId === route.modelId,
    );
    return option
      ? `${option.providerLabel} · ${option.modelLabel}`
      : "Selected route is unavailable";
  };

  function selectRoute(feature: string, value: string) {
    const parsed = parseServiceRouteValue(value);
    if (!parsed) {
      return;
    }
    props.onSelectFeatureRoute(feature, parsed.providerId, parsed.modelId);
  }

  return (
    <div class="settings-pane__inner">
      <div class="settings-pane__intro">
        <h2>Services</h2>
        <p>
          Choose provider-backed tools that every chat model can use, independent
          of the selected chat model.
        </p>
      </div>

      <Show
        when={!props.loading || serviceGroups().length > 0}
        fallback={<div class="settings-empty">Loading services...</div>}
      >
        <section class="settings-card service-routes">
          <div class="settings-card__head">
            <h3>Service routes</h3>
            <p>Image, audio, and speech features are routed here.</p>
          </div>

          <Show
            when={serviceGroups().length > 0}
            fallback={
              <p class="muted-line">
                No image, audio, or speech service models are available yet.
              </p>
            }
          >
            <div class="service-routes__list">
              <For each={serviceGroups()}>
                {(group) => (
                  <label class="service-route-row">
                    <span class="service-route-row__label">
                      <strong>{group.label}</strong>
                      <span>{selectedHint(group)}</span>
                    </span>
                    <select
                      class="settings-select"
                      value={selectedValue(group)}
                      onChange={(event) =>
                        selectRoute(group.feature, event.currentTarget.value)
                      }
                    >
                      <option value="" disabled>
                        {selectedRoute(group.feature)
                          ? "Selected route unavailable"
                          : "Not selected"}
                      </option>
                      <For each={group.options}>
                        {(option) => (
                          <option
                            value={serviceRouteValue(
                              option.providerId,
                              option.modelId,
                            )}
                          >
                            {option.providerLabel} · {option.modelLabel}
                            {option.recommended ? " · recommended" : ""}
                          </option>
                        )}
                      </For>
                    </select>
                  </label>
                )}
              </For>
            </div>
          </Show>
        </section>
      </Show>
    </div>
  );
}

function serviceLabel(feature: string, fallback: string) {
  switch (feature) {
    case "media.image.generate":
      return "Image generation";
    case "media.image.edit":
      return "Image editing";
    case "audio.transcribe":
      return "STT";
    case "audio.speech":
      return "Speech";
    default:
      return fallback || feature;
  }
}

function providerCanServeServices(provider: ConnectorProviderSummary) {
  if (!provider.enabled || provider.modelError) {
    return false;
  }
  if (provider.authKind === "none") {
    return true;
  }
  return provider.authenticated;
}

function serviceRouteValue(providerId: string, modelId: string) {
  return `${providerId}${SERVICE_ROUTE_VALUE_SEPARATOR}${modelId}`;
}

function parseServiceRouteValue(value: string) {
  const separatorIndex = value.indexOf(SERVICE_ROUTE_VALUE_SEPARATOR);
  if (separatorIndex < 1 || separatorIndex === value.length - 1) {
    return undefined;
  }
  return {
    providerId: value.slice(0, separatorIndex),
    modelId: value.slice(separatorIndex + SERVICE_ROUTE_VALUE_SEPARATOR.length),
  };
}

function compareServiceRouteGroups(
  left: ServiceRouteGroup,
  right: ServiceRouteGroup,
) {
  const leftRank = serviceFeatureRank(left.feature);
  const rightRank = serviceFeatureRank(right.feature);
  return leftRank === rightRank
    ? left.label.localeCompare(right.label)
    : leftRank - rightRank;
}

function compareServiceRouteOptions(
  left: ServiceRouteOption,
  right: ServiceRouteOption,
) {
  if (left.recommended !== right.recommended) {
    return left.recommended ? -1 : 1;
  }

  const providerOrder = left.providerLabel.localeCompare(right.providerLabel);
  return providerOrder === 0
    ? left.modelLabel.localeCompare(right.modelLabel)
    : providerOrder;
}

function serviceFeatureRank(feature: string) {
  switch (feature) {
    case "media.image.generate":
      return 0;
    case "media.image.edit":
      return 1;
    case "audio.transcribe":
      return 2;
    case "audio.speech":
      return 3;
    default:
      return 100;
  }
}
