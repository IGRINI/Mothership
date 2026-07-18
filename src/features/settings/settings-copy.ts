import { uiLocale, type UiLocale } from "../../shared/locale";

export type SettingsTabId =
  | "appearance"
  | "chat"
  | "connectors"
  | "personalization"
  | "permissions"
  | "services";

export interface SettingsSearchDocument {
  id: string;
  tabId: SettingsTabId;
  title: string;
  description: string;
  keywords: string[];
}

export interface SettingsCopy {
  shell: {
    title: string;
    back: string;
    refreshConnectors: string;
    searchPlaceholder: string;
    searchShortcut: string;
    modelSelectionSaved: string;
    serviceSelectionSaved: string;
    adapterSettingsSaved: string;
    connectorEnabled: string;
    connectorDisabled: string;
    authorizing: string;
    authorized: string;
    authorizationCancelled: string;
    loggedOut: string;
  };
  nav: {
    groups: { id: string; label: string }[];
    tabs: Record<SettingsTabId, string>;
  };
  search: {
    title: string;
    placeholder: string;
    empty: string;
    hint: string;
    noMatches: string;
    openHint: string;
    documents: SettingsSearchDocument[];
  };
  common: {
    loading: string;
    save: string;
    saving: string;
    add: string;
    remove: string;
    noneYet: string;
    enabled: string;
    disabled: string;
    settings: string;
    recommended: string;
    on: string;
    off: string;
  };
  appearance: {
    title: string;
    intro: string;
    interfaceLanguageTitle: string;
    interfaceLanguageDescription: string;
    modeTitle: string;
    modeDescription: string;
    paletteTitle: string;
    paletteDescription: string;
    accentTitle: string;
    accentDescription: string;
    typographyTitle: string;
    typographyDescription: string;
    interfaceFont: string;
    codeFont: string;
    scaleTitle: string;
    scaleDescription: string;
    defaultState: string;
    customState: string;
    reset: string;
    themeOptions: Record<string, { label: string; description: string }>;
    paletteOptions: Record<string, { label: string; description: string }>;
    accentOptions: Record<string, string>;
    fontOptions: Record<string, string>;
  };
  chat: {
    title: string;
    intro: string;
    previewTitle: string;
    previewDescription: string;
    messageFontTitle: string;
    messageFontDescription: string;
    font: string;
    sameAsInterface: string;
    messageSize: string;
    overrideSize: string;
    defaultSize: string;
    customSize: (value: number) => string;
    layoutTitle: string;
    layoutDescription: string;
    hideAvatar: string;
    hideAvatarDescription: string;
    hideModelName: string;
    hideModelNameDescription: string;
    collapseWork: string;
    collapseWorkDescription: string;
  };
  connectors: {
    title: string;
    intro: string;
    loading: string;
    empty: string;
    settings: string;
    enable: (label: string) => string;
    modelsTitle: string;
    modelCount: (count: number) => string;
    agentRuntime: string;
    connector: string;
    back: string;
    disabledHint: string;
    authorization: string;
    authorize: string;
    logout: string;
    cancel: string;
    unavailable: (error: string) => string;
    loadingModels: string;
    noModelsAuthorize: string;
    noModels: string;
    adapterSettings: string;
    clearSavedSecret: string;
    leaveBlankSecret: string;
    noSecretSaved: string;
    savedSecret: string;
    savedSecretLast4: (last4: string) => string;
    addModel: string;
    modelPlaceholder: string;
    saveSettings: string;
    authAuthorized: string;
    authAuthorize: string;
    signedInAs: (account: string) => string;
    statusDisabled: string;
    statusUpdating: string;
    statusUnavailable: string;
    statusNotConnected: string;
    statusNeedsApiKey: string;
    statusConnected: string;
    statusConnectedAs: (account: string) => string;
    statusWaiting: string;
    statusReady: string;
  };
  services: {
    title: string;
    intro: string;
    loading: string;
    routesTitle: string;
    routesDescription: string;
    empty: string;
    noRoute: string;
    unavailableRoute: string;
    notSelected: string;
    imageGeneration: string;
    imageEditing: string;
    stt: string;
    speech: string;
  };
  personalization: {
    title: string;
    intro: string;
    responseLanguageTitle: string;
    responseLanguageDescription: string;
    responseLanguageSelect: string;
    customLanguage: string;
    customLanguagePlaceholder: string;
    responseLanguageSaved: string;
    responseLanguageChatNote: string;
    scopeTitle: string;
    scopeDescription: string;
    global: string;
    perProvider: string;
    perModel: string;
    noConnectors: string;
    provider: string;
    model: string;
    noModels: string;
    customInstructionsTitle: string;
    pickScope: string;
    placeholder: string;
    characters: (count: number) => string;
    clear: string;
    saved: string;
    cleared: string;
    configuredTitle: string;
    configuredDescription: string;
    labels: {
      globalScope: string;
      globalApplied: string;
      provider: (label: string) => string;
      model: (provider: string, model: string) => string;
    };
    responseLanguages: Record<string, string>;
  };
  permissions: {
    title: string;
    intro: string;
    allowTitle: string;
    allowDescription: string;
    denyTitle: string;
    denyDescription: string;
    changeJournalTitle: string;
    changeJournalDescription: string;
    keepEverything: string;
    keepLast: (count: number) => string;
    unlimitedHistory: string;
    changesFromLast: (count: number) => string;
    toolAccessTitle: string;
    toolAccessDescription: string;
    rulesSaved: string;
    allowPlaceholder: string;
    denyPlaceholder: string;
    toolCatalog: Record<string, { label: string; description: string }>;
  };
}

const en: SettingsCopy = {
  shell: {
    title: "Settings",
    back: "Back",
    refreshConnectors: "Refresh connectors",
    searchPlaceholder: "Search settings",
    searchShortcut: "Ctrl+K",
    modelSelectionSaved: "Model selection saved.",
    serviceSelectionSaved: "Service selection saved.",
    adapterSettingsSaved: "Adapter settings saved.",
    connectorEnabled: "Connector enabled.",
    connectorDisabled: "Connector disabled.",
    authorizing: "Authorizing - finish the flow in your browser...",
    authorized: "Authorized.",
    authorizationCancelled: "Authorization cancelled.",
    loggedOut: "Logged out.",
  },
  nav: {
    groups: [
      { id: "general", label: "General" },
      { id: "providers", label: "Providers" },
      { id: "tools", label: "Tools & safety" },
    ],
    tabs: {
      appearance: "Appearance",
      chat: "Chat",
      connectors: "Connectors",
      services: "Services",
      personalization: "Personalization",
      permissions: "Permissions",
    },
  },
  search: {
    title: "Search settings",
    placeholder: "Search by name, option, meaning, or tag",
    empty: "Type to search every Settings section.",
    hint: "Enter opens the first match. Esc closes.",
    noMatches: "No settings match this search.",
    openHint: "Open",
    documents: [],
  },
  common: {
    loading: "Loading...",
    save: "Save",
    saving: "Saving...",
    add: "Add",
    remove: "Remove",
    noneYet: "None yet.",
    enabled: "Enabled",
    disabled: "Disabled",
    settings: "Settings",
    recommended: "recommended",
    on: "On",
    off: "Off",
  },
  appearance: {
    title: "Appearance",
    intro:
      "Tune the look of Mothership. These preferences are stored on this device and apply instantly across the whole app.",
    interfaceLanguageTitle: "Interface language",
    interfaceLanguageDescription:
      "Switch Settings between English and Russian. The rest of the app can move onto the same locale layer incrementally.",
    modeTitle: "Mode",
    modeDescription: "Light, dark, or follow the operating system.",
    paletteTitle: "Palette",
    paletteDescription: "The surface color flavor - applies in both light and dark.",
    accentTitle: "Accent",
    accentDescription: "The highlight color for buttons, selections, and focus.",
    typographyTitle: "Typography",
    typographyDescription: "Pick the interface and code typefaces.",
    interfaceFont: "Interface font",
    codeFont: "Code font",
    scaleTitle: "UI scale",
    scaleDescription:
      "Zoom the interface. The window controls and status bar stay at their native size.",
    defaultState: "Using the default appearance.",
    customState: "Custom appearance.",
    reset: "Reset to defaults",
    themeOptions: {
      system: {
        label: "System",
        description: "Follow the operating system's light or dark setting.",
      },
      light: {
        label: "Light",
        description: "A bright, low-contrast surface for well-lit rooms.",
      },
      dark: {
        label: "Dark",
        description: "A deep, low-glare surface for dim rooms.",
      },
    },
    paletteOptions: {
      aurora: { label: "Aurora", description: "Cool cyan surfaces with a bright blue accent range." },
      midnight: { label: "Midnight", description: "Neutral dark surfaces with restrained contrast." },
      graphite: { label: "Graphite", description: "Gray workbench surfaces with a low-color feel." },
      nebula: { label: "Nebula", description: "Soft violet-tinted surfaces for a livelier workspace." },
    },
    accentOptions: {
      azure: "Azure",
      iris: "Iris",
      emerald: "Emerald",
      amber: "Amber",
      rose: "Rose",
      cyan: "Cyan",
    },
    fontOptions: {
      system: "System",
      inter: "Inter",
      segoe: "Segoe UI",
      verdana: "Verdana",
      georgia: "Georgia",
      cascadia: "Cascadia",
      consolas: "Consolas",
      jetbrains: "JetBrains Mono",
      courier: "Courier",
    },
  },
  chat: {
    title: "Chat",
    intro:
      "Control how chat messages look. These preferences are stored on this device and apply live - the example below updates as you change them.",
    previewTitle: "Preview",
    previewDescription: "A sample exchange rendered with your current settings.",
    messageFontTitle: "Message font",
    messageFontDescription:
      "Use the interface font from Appearance, or pick a different one.",
    font: "Font",
    sameAsInterface: "Same as interface",
    messageSize: "Message size",
    overrideSize: "Override size",
    defaultSize: "Using the default chat size.",
    customSize: (value) => `Custom: ${value}px`,
    layoutTitle: "Message layout",
    layoutDescription: "Trim the assistant byline to taste.",
    hideAvatar: "Hide provider avatar",
    hideAvatarDescription: "Drop the provider icon next to assistant replies.",
    hideModelName: "Hide model name",
    hideModelNameDescription: "Drop the model label above each assistant reply.",
    collapseWork: "Collapse work under a spoiler",
    collapseWorkDescription:
      "Hide every tool call and intermediate step behind one collapsible header, leaving only the final answer.",
  },
  connectors: {
    title: "Connectors",
    intro:
      "Each provider is a runtime-loaded adapter that authorizes itself. Turn one off to hide it from model selection without losing its setup; open Settings to authorize, pick models, and configure it.",
    loading: "Loading connectors...",
    empty:
      "No connectors found. Restart the app; if this keeps happening, reinstall Mothership.",
    settings: "Settings",
    enable: (label) => `Enable ${label}`,
    modelsTitle: "Models",
    modelCount: (count) => `${count} ${count === 1 ? "model" : "models"}`,
    agentRuntime: "Agent runtime",
    connector: "Connector",
    back: "Connectors",
    disabledHint:
      "This connector is turned off, so it won't appear when choosing a model. Turn it on to use it in chat - your setup below is kept either way.",
    authorization: "Authorization",
    authorize: "Authorize",
    logout: "Log out",
    cancel: "Cancel",
    unavailable: (error) => `Connector unavailable: ${error}`,
    loadingModels: "Loading models...",
    noModelsAuthorize: "No models loaded - authorize to fetch them.",
    noModels: "No models loaded.",
    adapterSettings: "Settings",
    clearSavedSecret: "Clear saved secret",
    leaveBlankSecret: "Leave blank to keep saved secret",
    noSecretSaved: "No secret saved.",
    savedSecret: "Saved secret configured. Leave empty to keep it.",
    savedSecretLast4: (last4) => `Saved secret ending in ${last4}. Leave empty to keep it.`,
    addModel: "Add model",
    modelPlaceholder: "provider/model-id",
    saveSettings: "Save settings",
    authAuthorized: "Authorized.",
    authAuthorize: "Authorize this connector to fetch its models.",
    signedInAs: (account) => `Signed in as ${account}.`,
    statusDisabled: "Disabled",
    statusUpdating: "Updating models...",
    statusUnavailable: "Unavailable",
    statusNotConnected: "Not connected",
    statusNeedsApiKey: "Needs API key",
    statusConnected: "Connected",
    statusConnectedAs: (account) => `Connected · ${account}`,
    statusWaiting: "Waiting for connector...",
    statusReady: "Ready",
  },
  services: {
    title: "Services",
    intro:
      "Choose provider-backed tools that every chat model can use, independent of the selected chat model.",
    loading: "Loading services...",
    routesTitle: "Service routes",
    routesDescription: "Image, audio, and speech features are routed here.",
    empty: "No image, audio, or speech service models are available yet.",
    noRoute: "No route selected",
    unavailableRoute: "Selected route is unavailable",
    notSelected: "Not selected",
    imageGeneration: "Image generation",
    imageEditing: "Image editing",
    stt: "STT",
    speech: "Speech",
  },
  personalization: {
    title: "Personalization",
    intro:
      "Append your own instructions to Mothership's system prompt. Set a global baseline, then layer more specific overrides per provider or per model - all applicable scopes are added together, broad to specific.",
    responseLanguageTitle: "Response language",
    responseLanguageDescription:
      "Set the global default language for model answers. Auto adds no prompt instruction.",
    responseLanguageSelect: "Language",
    customLanguage: "Custom language",
    customLanguagePlaceholder: "e.g. Brazilian Portuguese, simple Russian",
    responseLanguageSaved: "Response language saved.",
    responseLanguageChatNote:
      "Chat-level overrides will live in chat settings later; this is the global default.",
    scopeTitle: "Scope",
    scopeDescription: "Choose what these instructions apply to.",
    global: "Global",
    perProvider: "Per provider",
    perModel: "Per model",
    noConnectors:
      "No connectors are loaded yet. Open the Connectors tab and authorize a provider first.",
    provider: "Provider",
    model: "Model",
    noModels: "This provider has no models loaded yet.",
    customInstructionsTitle: "Custom instructions",
    pickScope: "Pick a provider and model to edit this scope.",
    placeholder:
      "e.g. Always respond in Russian. Prefer concise answers and explain trade-offs before recommending one option.",
    characters: (count) => `${count} characters`,
    clear: "Clear",
    saved: "Custom instructions saved.",
    cleared: "Custom instructions cleared.",
    configuredTitle: "Configured scopes",
    configuredDescription: "Everything you've personalized so far.",
    labels: {
      globalScope: "Global",
      globalApplied: "Global - applied to every model",
      provider: (label) => `Provider - ${label}`,
      model: (provider, model) => `Model - ${provider} · ${model}`,
    },
    responseLanguages: {
      auto: "Auto",
      en: "English",
      ru: "Russian",
      es: "Spanish",
      de: "German",
      fr: "French",
      it: "Italian",
      pt: "Portuguese",
      zh: "Chinese",
      ja: "Japanese",
      ko: "Korean",
      uk: "Ukrainian",
      pl: "Polish",
      tr: "Turkish",
      ar: "Arabic",
      hi: "Hindi",
      custom: "Custom...",
    },
  },
  permissions: {
    title: "Permissions",
    intro:
      "Control how the agent uses tools: an allowlist / denylist for shell commands, and which tools are available at all. The approval mode (manual / auto / yolo) is per-chat - set it under the chat's input.",
    allowTitle: "Command allowlist",
    allowDescription:
      "Programs here are auto-approved - they skip the approval prompt regardless of mode. Match is by name.",
    denyTitle: "Command denylist",
    denyDescription:
      "Programs here are always blocked, even in Yolo mode. Deny wins over allow if a program is in both.",
    changeJournalTitle: "Change journal",
    changeJournalDescription:
      "Every file change an agent makes is snapshotted so it can be reviewed and reverted. History is kept for the last N agent messages per project; 0 keeps everything.",
    keepEverything: "Change journal: keeping everything.",
    keepLast: (count) =>
      `Change journal: keeping changes from the last ${count} messages per project.`,
    unlimitedHistory: "Unlimited history",
    changesFromLast: (count) => `Changes from the last ${count} messages per project`,
    toolAccessTitle: "Tool access",
    toolAccessDescription: "Turn individual tools off so the agent can never call them.",
    rulesSaved: "Permission rules saved.",
    allowPlaceholder: "e.g. npm",
    denyPlaceholder: "e.g. rm",
    toolCatalog: {
      run_command: { label: "Run commands", description: "Execute local shell / OS commands." },
      read_file: { label: "Read files", description: "Read a workspace file (read-only)." },
      write_file: { label: "Write files", description: "Create or overwrite a workspace file." },
      edit_file: { label: "Edit files", description: "Content-addressed string replacement in a file." },
      apply_patch: { label: "Apply patches", description: "Multi-file patch applied all-or-nothing." },
      search_text: { label: "Search text", description: "Content search across the workspace (read-only)." },
    },
  },
};

const ru: SettingsCopy = {
  ...en,
  shell: {
    title: "Настройки",
    back: "Назад",
    refreshConnectors: "Обновить коннекторы",
    searchPlaceholder: "Поиск настроек",
    searchShortcut: "Ctrl+K",
    modelSelectionSaved: "Выбор модели сохранён.",
    serviceSelectionSaved: "Выбор сервиса сохранён.",
    adapterSettingsSaved: "Настройки адаптера сохранены.",
    connectorEnabled: "Коннектор включён.",
    connectorDisabled: "Коннектор отключён.",
    authorizing: "Идёт авторизация - заверши её в браузере...",
    authorized: "Авторизовано.",
    authorizationCancelled: "Авторизация отменена.",
    loggedOut: "Выход выполнен.",
  },
  nav: {
    groups: [
      { id: "general", label: "Общее" },
      { id: "providers", label: "Провайдеры" },
      { id: "tools", label: "Инструменты и безопасность" },
    ],
    tabs: {
      appearance: "Внешний вид",
      chat: "Чат",
      connectors: "Коннекторы",
      services: "Сервисы",
      personalization: "Персонализация",
      permissions: "Разрешения",
    },
  },
  search: {
    title: "Поиск настроек",
    placeholder: "Ищи по названию, опции, смыслу или тегу",
    empty: "Начни вводить запрос, чтобы искать по всем настройкам.",
    hint: "Enter откроет первое совпадение. Esc закроет окно.",
    noMatches: "Ничего не найдено.",
    openHint: "Открыть",
    documents: [],
  },
  common: {
    loading: "Загрузка...",
    save: "Сохранить",
    saving: "Сохранение...",
    add: "Добавить",
    remove: "Удалить",
    noneYet: "Пока пусто.",
    enabled: "Включено",
    disabled: "Отключено",
    settings: "Настройки",
    recommended: "рекомендуется",
    on: "Вкл",
    off: "Выкл",
  },
  appearance: {
    ...en.appearance,
    title: "Внешний вид",
    intro:
      "Настрой внешний вид Mothership. Эти параметры сохраняются на этом устройстве и сразу применяются ко всему приложению.",
    interfaceLanguageTitle: "Язык интерфейса",
    interfaceLanguageDescription:
      "Переключает Settings между русским и английским. Остальные части приложения можно постепенно перевести через этот же слой.",
    modeTitle: "Тема",
    modeDescription: "Светлая, тёмная или как в операционной системе.",
    paletteTitle: "Палитра",
    paletteDescription: "Цветовой характер поверхностей - работает и в светлой, и в тёмной теме.",
    accentTitle: "Акцент",
    accentDescription: "Цвет выделений, кнопок, выбора и фокуса.",
    typographyTitle: "Типографика",
    typographyDescription: "Выбор шрифта интерфейса и кода.",
    interfaceFont: "Шрифт интерфейса",
    codeFont: "Шрифт кода",
    scaleTitle: "Масштаб UI",
    scaleDescription:
      "Масштабирует интерфейс. Оконные элементы и статусная строка остаются в системном размере.",
    defaultState: "Используется внешний вид по умолчанию.",
    customState: "Внешний вид настроен вручную.",
    reset: "Сбросить",
    themeOptions: {
      system: { label: "Системная", description: "Следовать светлой или тёмной теме ОС." },
      light: { label: "Светлая", description: "Светлая спокойная поверхность для яркого окружения." },
      dark: { label: "Тёмная", description: "Глубокая поверхность с низкой нагрузкой на глаза." },
    },
    paletteOptions: {
      aurora: { label: "Аврора", description: "Холодные cyan-поверхности с ярким синим акцентом." },
      midnight: { label: "Полночь", description: "Нейтральные тёмные поверхности со сдержанным контрастом." },
      graphite: { label: "Графит", description: "Серый рабочий стол с минимумом цветового шума." },
      nebula: { label: "Небула", description: "Мягкий фиолетовый оттенок для более живой рабочей среды." },
    },
    accentOptions: {
      azure: "Лазурный",
      iris: "Ирис",
      emerald: "Изумрудный",
      amber: "Янтарный",
      rose: "Розовый",
      cyan: "Циан",
    },
    fontOptions: en.appearance.fontOptions,
  },
  chat: {
    ...en.chat,
    title: "Чат",
    intro:
      "Настрой отображение сообщений. Эти параметры сохраняются на устройстве и применяются сразу - пример ниже обновляется на лету.",
    previewTitle: "Предпросмотр",
    previewDescription: "Пример диалога с текущими настройками.",
    messageFontTitle: "Шрифт сообщений",
    messageFontDescription: "Можно использовать шрифт интерфейса или выбрать отдельный.",
    font: "Шрифт",
    sameAsInterface: "Как в интерфейсе",
    messageSize: "Размер сообщений",
    overrideSize: "Переопределить размер",
    defaultSize: "Используется стандартный размер чата.",
    customSize: (value) => `Свой размер: ${value}px`,
    layoutTitle: "Компоновка сообщений",
    layoutDescription: "Настрой служебную строку ассистента.",
    hideAvatar: "Скрыть аватар провайдера",
    hideAvatarDescription: "Убрать иконку провайдера рядом с ответами ассистента.",
    hideModelName: "Скрыть название модели",
    hideModelNameDescription: "Убрать подпись модели над ответами ассистента.",
    collapseWork: "Сворачивать работу под спойлер",
    collapseWorkDescription:
      "Скрывать вызовы инструментов и промежуточные шаги под одним раскрываемым заголовком, оставляя финальный ответ.",
  },
  connectors: {
    ...en.connectors,
    title: "Коннекторы",
    intro:
      "Каждый провайдер подключается как runtime adapter и сам проходит авторизацию. Отключение скрывает его из выбора моделей, но не удаляет настройку; открой настройки, чтобы авторизоваться, выбрать модели и параметры.",
    loading: "Загрузка коннекторов...",
    empty:
      "Коннекторы не найдены. Перезапусти приложение; если это повторяется, переустанови Mothership.",
    settings: "Настройки",
    enable: (label) => `Включить ${label}`,
    modelsTitle: "Модели",
    modelCount: (count) => `${count} ${count === 1 ? "модель" : "моделей"}`,
    agentRuntime: "Agent runtime",
    connector: "Коннектор",
    back: "Коннекторы",
    disabledHint:
      "Коннектор отключён и не появится при выборе модели. Включи его, чтобы использовать в чате; текущая настройка сохранится.",
    authorization: "Авторизация",
    authorize: "Авторизоваться",
    logout: "Выйти",
    cancel: "Отменить",
    unavailable: (error) => `Коннектор недоступен: ${error}`,
    loadingModels: "Загрузка моделей...",
    noModelsAuthorize: "Модели не загружены - авторизуйся, чтобы получить список.",
    noModels: "Модели не загружены.",
    adapterSettings: "Настройки",
    clearSavedSecret: "Удалить сохранённый секрет",
    leaveBlankSecret: "Оставь пустым, чтобы сохранить текущий секрет",
    noSecretSaved: "Секрет не сохранён.",
    savedSecret: "Секрет настроен. Оставь поле пустым, чтобы сохранить его.",
    savedSecretLast4: (last4) => `Секрет сохранён, последние символы: ${last4}. Оставь пустым, чтобы сохранить его.`,
    addModel: "Добавить модель",
    modelPlaceholder: "provider/model-id",
    saveSettings: "Сохранить настройки",
    authAuthorized: "Авторизовано.",
    authAuthorize: "Авторизуй коннектор, чтобы загрузить модели.",
    signedInAs: (account) => `Вход выполнен как ${account}.`,
    statusDisabled: "Отключён",
    statusUpdating: "Обновление моделей...",
    statusUnavailable: "Недоступен",
    statusNotConnected: "Не подключён",
    statusNeedsApiKey: "Нужен API key",
    statusConnected: "Подключён",
    statusConnectedAs: (account) => `Подключён · ${account}`,
    statusWaiting: "Ожидание коннектора...",
    statusReady: "Готов",
  },
  services: {
    ...en.services,
    title: "Сервисы",
    intro:
      "Выбери провайдерские инструменты, доступные любой чат-модели независимо от выбранной модели чата.",
    loading: "Загрузка сервисов...",
    routesTitle: "Маршруты сервисов",
    routesDescription: "Здесь настраиваются изображения, аудио и речь.",
    empty: "Пока нет доступных моделей для изображений, аудио или речи.",
    noRoute: "Маршрут не выбран",
    unavailableRoute: "Выбранный маршрут недоступен",
    notSelected: "Не выбрано",
    imageGeneration: "Генерация изображений",
    imageEditing: "Редактирование изображений",
    stt: "Распознавание речи",
    speech: "Синтез речи",
  },
  personalization: {
    ...en.personalization,
    title: "Персонализация",
    intro:
      "Добавь свои инструкции в системный prompt Mothership. Можно задать глобальную базу, а потом уточнить её для провайдера или модели; применимые уровни складываются от общего к частному.",
    responseLanguageTitle: "Язык ответа",
    responseLanguageDescription:
      "Глобальный язык ответов модели. Auto не добавляет отдельную prompt-инструкцию.",
    responseLanguageSelect: "Язык",
    customLanguage: "Свой язык",
    customLanguagePlaceholder: "например: Brazilian Portuguese, простой русский",
    responseLanguageSaved: "Язык ответа сохранён.",
    responseLanguageChatNote:
      "Переопределение на уровне чата будет в настройках чата позже; сейчас это глобальный дефолт.",
    scopeTitle: "Область",
    scopeDescription: "Выбери, к чему применяются инструкции.",
    global: "Глобально",
    perProvider: "На провайдера",
    perModel: "На модель",
    noConnectors:
      "Коннекторы ещё не загружены. Открой вкладку Коннекторы и авторизуй провайдера.",
    provider: "Провайдер",
    model: "Модель",
    noModels: "У этого провайдера пока нет загруженных моделей.",
    customInstructionsTitle: "Пользовательские инструкции",
    pickScope: "Выбери провайдера и модель, чтобы редактировать эту область.",
    placeholder:
      "например: Всегда отвечай на русском. Пиши кратко и объясняй компромиссы перед рекомендацией.",
    characters: (count) => `${count} символов`,
    clear: "Очистить",
    saved: "Пользовательские инструкции сохранены.",
    cleared: "Пользовательские инструкции очищены.",
    configuredTitle: "Настроенные области",
    configuredDescription: "Все сохранённые правила персонализации.",
    labels: {
      globalScope: "Глобально",
      globalApplied: "Глобально - применяется ко всем моделям",
      provider: (label) => `Провайдер - ${label}`,
      model: (provider, model) => `Модель - ${provider} · ${model}`,
    },
    responseLanguages: {
      auto: "Авто",
      en: "Английский",
      ru: "Русский",
      es: "Испанский",
      de: "Немецкий",
      fr: "Французский",
      it: "Итальянский",
      pt: "Португальский",
      zh: "Китайский",
      ja: "Японский",
      ko: "Корейский",
      uk: "Украинский",
      pl: "Польский",
      tr: "Турецкий",
      ar: "Арабский",
      hi: "Хинди",
      custom: "Свой...",
    },
  },
  permissions: {
    ...en.permissions,
    title: "Разрешения",
    intro:
      "Управляй тем, как агент использует инструменты: allowlist/denylist для shell-команд и доступность отдельных tools. Режим подтверждений (manual / auto / yolo) задаётся на уровне чата под полем ввода.",
    allowTitle: "Allowlist команд",
    allowDescription:
      "Программы здесь подтверждаются автоматически и пропускают запрос разрешения независимо от режима. Сравнение идёт по имени.",
    denyTitle: "Denylist команд",
    denyDescription:
      "Программы здесь всегда блокируются, даже в Yolo mode. Deny сильнее allow, если программа есть в обоих списках.",
    changeJournalTitle: "Журнал изменений",
    changeJournalDescription:
      "Каждое изменение файлов агентом сохраняется для просмотра и отката. История хранится для последних N сообщений агента на проект; 0 хранит всё.",
    keepEverything: "Журнал изменений: хранится всё.",
    keepLast: (count) =>
      `Журнал изменений: хранятся изменения последних ${count} сообщений на проект.`,
    unlimitedHistory: "История без ограничения",
    changesFromLast: (count) => `Изменения последних ${count} сообщений на проект`,
    toolAccessTitle: "Доступ к инструментам",
    toolAccessDescription: "Отключи отдельные tools, чтобы агент вообще не мог их вызвать.",
    rulesSaved: "Правила разрешений сохранены.",
    allowPlaceholder: "например: npm",
    denyPlaceholder: "например: rm",
    toolCatalog: {
      run_command: { label: "Запуск команд", description: "Выполнять локальные shell / OS команды." },
      read_file: { label: "Чтение файлов", description: "Читать файл рабочего проекта без изменений." },
      write_file: { label: "Запись файлов", description: "Создавать или перезаписывать файл проекта." },
      edit_file: { label: "Редактирование файлов", description: "Замена строк в файле с проверкой содержимого." },
      apply_patch: { label: "Патчи", description: "Многофайловый patch, применяемый целиком или никак." },
      search_text: { label: "Поиск текста", description: "Поиск по содержимому проекта без изменений." },
    },
  },
};

en.search.documents = searchDocuments(en);
ru.search.documents = searchDocuments(ru);

const COPY_BY_LOCALE: Record<UiLocale, SettingsCopy> = { en, ru };

export function settingsCopy(): SettingsCopy {
  return COPY_BY_LOCALE[uiLocale()];
}

function searchDocuments(copy: SettingsCopy): SettingsSearchDocument[] {
  return [
    doc("appearance.language", "appearance", copy.appearance.interfaceLanguageTitle, copy.appearance.interfaceLanguageDescription, [
      "locale", "localization", "i18n", "translation", "language", "язык", "локализация", "перевод",
    ]),
    doc("appearance.mode", "appearance", copy.appearance.modeTitle, copy.appearance.modeDescription, [
      "theme", "dark", "light", "system", "тема", "темная", "светлая", "jetbrains",
    ]),
    doc("appearance.palette", "appearance", copy.appearance.paletteTitle, copy.appearance.paletteDescription, [
      "aurora", "midnight", "graphite", "nebula", "colors", "palette", "цвета", "палитра",
    ]),
    doc("appearance.accent", "appearance", copy.appearance.accentTitle, copy.appearance.accentDescription, [
      "accent", "azure", "iris", "emerald", "amber", "rose", "cyan", "акцент", "выделение",
    ]),
    doc("appearance.typography", "appearance", copy.appearance.typographyTitle, copy.appearance.typographyDescription, [
      "font", "typeface", "mono", "jetbrains mono", "шрифт", "код",
    ]),
    doc("appearance.scale", "appearance", copy.appearance.scaleTitle, copy.appearance.scaleDescription, [
      "zoom", "scale", "size", "масштаб", "размер",
    ]),
    doc("chat.preview", "chat", copy.chat.previewTitle, copy.chat.previewDescription, [
      "sample", "preview", "пример", "предпросмотр",
    ]),
    doc("chat.message-font", "chat", copy.chat.messageFontTitle, copy.chat.messageFontDescription, [
      "font", "message size", "chat text", "шрифт", "сообщения", "размер",
    ]),
    doc("chat.layout", "chat", copy.chat.layoutTitle, copy.chat.layoutDescription, [
      "avatar", "model name", "collapse work", "spoiler", "аватар", "модель", "спойлер",
    ]),
    doc("connectors.overview", "connectors", copy.connectors.title, copy.connectors.intro, [
      "provider", "adapter", "oauth", "api key", "model", "провайдер", "адаптер", "ключ",
    ]),
    doc("connectors.authorization", "connectors", copy.connectors.authorization, copy.connectors.authAuthorize, [
      "login", "logout", "authorize", "account", "войти", "авторизация", "аккаунт",
    ]),
    doc("connectors.models", "connectors", copy.connectors.modelsTitle, copy.connectors.noModels, [
      "model picker", "hidden models", "visible models", "модели", "выбор модели",
    ]),
    doc("connectors.adapter-settings", "connectors", copy.connectors.adapterSettings, copy.connectors.saveSettings, [
      "adapter settings", "secret", "api key", "base url", "настройки адаптера", "секрет",
    ]),
    doc("services.routes", "services", copy.services.routesTitle, copy.services.routesDescription, [
      "image", "audio", "speech", "stt", "route", "service", "изображения", "аудио", "речь",
    ]),
    doc("personalization.response-language", "personalization", copy.personalization.responseLanguageTitle, copy.personalization.responseLanguageDescription, [
      "answer language", "response language", "model language", "нейронка", "ответ", "язык ответа",
    ]),
    doc("personalization.scope", "personalization", copy.personalization.scopeTitle, copy.personalization.scopeDescription, [
      "global", "provider", "model", "scope", "область", "провайдер", "модель",
    ]),
    doc("personalization.instructions", "personalization", copy.personalization.customInstructionsTitle, copy.personalization.placeholder, [
      "custom instructions", "prompt", "system prompt", "инструкции", "промпт", "персонализация",
    ]),
    doc("personalization.saved", "personalization", copy.personalization.configuredTitle, copy.personalization.configuredDescription, [
      "configured scopes", "saved", "rules", "сохраненные", "области", "правила",
    ]),
    doc("permissions.allowlist", "permissions", copy.permissions.allowTitle, copy.permissions.allowDescription, [
      "allow", "auto approve", "command", "shell", "авто", "разрешить", "команды",
    ]),
    doc("permissions.denylist", "permissions", copy.permissions.denyTitle, copy.permissions.denyDescription, [
      "deny", "block", "command", "yolo", "запрет", "блокировка", "команды",
    ]),
    doc("permissions.change-journal", "permissions", copy.permissions.changeJournalTitle, copy.permissions.changeJournalDescription, [
      "history", "journal", "revert", "snapshot", "изменения", "откат", "история",
    ]),
    doc("permissions.tool-access", "permissions", copy.permissions.toolAccessTitle, copy.permissions.toolAccessDescription, [
      "tools", "disable tool", "run_command", "read_file", "apply_patch", "инструменты", "tool",
    ]),
  ];
}

function doc(
  id: string,
  tabId: SettingsTabId,
  title: string,
  description: string,
  keywords: string[],
): SettingsSearchDocument {
  return { id, tabId, title, description, keywords };
}
