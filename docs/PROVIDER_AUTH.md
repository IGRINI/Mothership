# Provider Auth

Этот документ фиксирует, как Mothership интегрирует провайдеров и их
авторизацию.

После пивота на subprocess-адаптеры авторизация перестала быть отдельной
подсистемой Core: это одна из возможностей, которую адаптер декларирует через
общий контракт. Core полностью provider-agnostic. Каждый провайдер — отдельный
дочерний процесс (adapter), который сам владеет своей авторизацией, транспортом
и логикой моделей. Документ описывает реализованную архитектуру; предыдущая
версия описывала in-Core модель ProviderAuth и устарела.

## Архитектура

- Core ничего не знает про конкретных провайдеров (OpenAI, Anthropic, OAuth,
  API keys). Это правило жёсткое: никакого provider-specific кода в Core,
  sidecar или Tauri host.
- Каждый провайдер — отдельный дочерний процесс-адаптер. Он общается с host по
  newline-delimited JSON через stdio. Один и тот же контракт покрывает обычный
  HTTP-провайдер (процесс адаптера сам ходит в API) и провайдера, который
  запускает внешний CLI (процесс спавнит и супервизит CLI и мостит его в тот же
  поток) — Core видит их одинаково.
- Адаптер сам владеет: identity, списком моделей, schema настроек, авторизацией,
  транспортом (HTTP/WS/SSE или внешний процесс) и форматом chat-запроса.
- Адаптеры бывают двух runtime-классов:
  - `core_managed` — Core владеет agentic loop и tool execution, адаптер
    выполняет один provider round за раз (`codex`, `openrouter`);
  - `self_managed` — upstream agent runtime внутри адаптера владеет своим loop,
    tools, subagents, compaction и continuation state. Core запускает процесс,
    хранит auth/state, стримит output и отображает события. Первый такой
    адаптер — `claude-agent` на Claude Agent SDK.
- Секреты живут в общем app-level vault, ключом служит `provider_id`. Host
  отдаёт их адаптеру и принимает обратно, но остаётся provider-agnostic.
- Падение адаптера не роняет приложение: он в своём процессе, ошибка всплывает
  на следующем чтении из его stdout.

```text
mothership-core  (provider-agnostic)
  ChatRunService --> SubprocessChatGateway
                       |
mothership-adapter-host (spawn + JSON-over-stdio + StoreSecret side channel)
                       |
adapters/<provider>    (codex, openrouter, ...)  <-- own auth/transport/models
                       |
provider API           (HTTP / WS / SSE / внешний CLI)
```

Код:

- контракт и host: `crates/mothership-adapter-host` (`protocol.rs`, `lib.rs`);
- мост к chat: `crates/mothership-core/src/subprocess_gateway.rs`;
- оркестрация run: `crates/mothership-core/src/run.rs`;
- vault: `crates/mothership-core/src/auth/vault.rs`;
- адаптеры: `adapters/codex`, `adapters/openrouter`.

## Требования продукта

Эти требования зафиксированы явно. Их нельзя ослаблять без отдельного
продуктового решения.

- Mothership должен работать как самостоятельное приложение.
- Mothership не должен зависеть от Claude Code для Anthropic authorization.
- Mothership не должен читать credentials Claude Code или состояние `~/.claude`.
- OpenAI subscription access реализуется через Codex OAuth внутри адаптера.
- Anthropic subscription access реализуется через manual paste OAuth-токена.
  Mothership не запускает собственный OAuth flow для Anthropic. Подробнее —
  в разделе [Anthropic adapter](#anthropic-adapter-планируемый).
- Адаптеры и способы входа добавляются без изменения Core-концепций.
- API-key auth — это просто адаптер с auth-схемой `api_key`, без отдельного
  Core-пути.
- Сырые токены не должны попадать в UI state, chat messages, event logs, agent
  prompts или обычные logs.
- Агент/runtime использует провайдера через адаптер и его модель, а не через
  сырые credentials напрямую в бизнес-логике Core.

Исключение: self-managed upstream-agent adapters (например `claude-agent`) могут
запускать SDK/CLI runtime провайдера как дочерний headless-процесс. Это не
делает Claude Code владельцем Mothership Core: Core всё ещё хранит настройки,
секреты, выбранную модель, chat history, cancellation и UI events. Но tools и
agent loop внутри такого runtime считаются upstream-owned и не проходят через
Mothership `ToolSupervisor`.

## Контракт core↔adapter

Один универсальный контракт для всех адаптеров. Wire-формат — по одному JSON на
строку. Host шлёт `Request`, адаптер отвечает одним или несколькими `Outbound`,
повторяя `id` исходного запроса. Определения — в
`crates/mothership-adapter-protocol/src/lib.rs`.

Host → adapter (`Request`, поле `method`):

```text
initialize            { protocol_version }  рукопожатие + версия протокола
get_identity          provider_id + человекочитаемый label
get_models            список моделей + режим управления списком
get_settings_schema   поля настроек для UI
set_settings          { values }  текущие значения настроек (host шлёт при изменении)
get_auth_schema       схема авторизации
get_auth_status       provider-agnostic статус после применения текущих settings
authenticate          запустить свой auth-flow (например browser OAuth) сейчас
chat_start            { model, messages, state?, tool_results?, extra_messages? }  один model round
chat_cancel           отмена
logout                ревокнуть/почистить свой credential перед забыванием в vault
```

Adapter → host (`Outbound`, поле `type`):

```text
initialized     { protocol_version }  ответ на initialize: версия адаптера
ack             подтверждение set_settings/authenticate/logout
identity        { provider_id, provider_label }
models          { management, models }
settings_schema { fields }
auth_schema     { auth }
auth_status     { kind, account_label?, expires_at?, detail? }
delta           { text }     потоковый кусок ответа
chat_round_complete { state?, tool_calls[] }  конец model round
error           { message }
store_secret    { values }   side channel: сохранить секреты в общий vault (без id)
```

Версионирование: `PROTOCOL_VERSION` в `protocol.rs`. Host шлёт свою версию на
`initialize`, адаптер отвечает `initialized` со своей; при несовпадении host
отказывается работать с адаптером (а не молча мис-парсит контракт). Bump major
при любом несовместимом изменении.

Режим управления моделями (`ModelManagement`): `fixed` (встроенный список),
`server` (берётся с сервера провайдера, например Codex), `user_defined`
(пользователь ведёт список сам, UI показывает «+», например OpenRouter).

Поле настроек (`SettingsField`): `{ key, label, kind, required }`, где `kind` —
`text`, `secret`, `bool` или `string_list` (редактируемый список строк с +/−;
хранится как элементы, склеенные через `\n`). UI рендерит форму по schema и
возвращает значения через `set_settings`.

Схема авторизации (`AuthKind`):

```text
none              авторизация не нужна
api_key { label } пользователь вводит ключ (хранится как secret-настройка)
oauth_internal    адаптер сам выполняет OAuth
external_process  адаптер запускает и ведёт внешний процесс (например claude-code)
```

`StoreSecret` обрабатывается прозрачно внутри `Adapter::recv` и не всплывает
вызывающему: адаптер может сохранить credential в любой момент, в том числе
посреди чата, не ломая поток request/response.

Для `self_managed` adapters тот же `chat_start` используется как один запуск
upstream agent runtime. Адаптер возвращает `chat_round_complete.state` как
opaque provider state (например Claude session id), а `tool_calls` должен быть
пустым. Core сохраняет state на уровне chat thread и передаёт его следующему
запуску того же provider. Если пользователь редактирует старое сообщение и
ветка истории обрезается, Core очищает provider state.

`chat_start.runtime_context` несёт структурный project context (`projectRoot`,
`projectId`, `projectName`). Self-managed адаптеры используют его для cwd/рабочей
среды upstream runtime, а не парсят путь из prompt.

Статус авторизации (`AuthStatus`) остаётся provider-agnostic, но вычисляется
адаптером, потому что только адаптер знает форму своего credential:
`not_required`, `missing`, `configured`, `authenticated`, `expired`, `error`.
Core не интерпретирует OAuth/API-key payload, а только агрегирует этот статус в
connector snapshot.

## Discovery и манифест

Адаптеры лежат в каталоге плагинов приложения — `<data>/plugins/<name>/` — и
описываются манифестом `adapter.json`:

```json
{
  "provider_id": "openrouter",
  "provider_label": "OpenRouter",
  "program": "openrouter-adapter.exe"
}
```

`AdapterRegistry::scan(dir)` обходит подкаталоги, читает манифесты и резолвит
`program` относительно папки манифеста. Отсутствующий каталог — пустой реестр;
битый или нечитаемый манифест пропускается (не ломает остальные). `find(provider_id)`
маппит провайдера на исполняемый файл.

Сейчас адаптер спавнится на каждую операцию (список моделей, чтение schema,
один ход чата) и убивается на drop. Резидентные инстансы/пул — позже, если
важна стоимость старта.

## Секреты и общий vault

Все секреты приложения лежат в одном общем vault, реализованном
`FileCredentialVault` с корнем `<data>/auth`. Файлы — в `<data>/auth/credentials/`.
Vault — это конкретная структура (без трейтов и абстракций), хранящая
**настройки адаптера** (включая его секреты): файл `adapter_<provider>.json`,
доступ через методы `load_adapter_settings` / `save_adapter_settings` /
`merge_adapter_settings`. Это вся карта `set_settings` адаптера, в том числе
поля `secret` (API key) и, для Codex, целый JSON-credential под ключом
`credential`.

Поток настроек и секретов:

- процессы адаптеров короткоживущие, поэтому host грузит `adapter_<provider>.json`
  и пушит его адаптеру через `set_settings` на **каждом** спавне;
- если адаптер выпустил или обновил токен, он пушит его обратно через
  `StoreSecret`; handler host'а вызывает `vault.merge_adapter_settings`. Секрет
  попадает в **тот же** общий vault, а не рядом с адаптером на диске;
- `merge`-семантика: load → extend → save. Поэтому UI «Save» (знает только
  задекларированные поля) и `StoreSecret` (несёт только изменённые ключи) не
  затирают данные друг друга.

Запись атомарная: temp-файл + rename, с бэкапом при замене. Schema version — 1.

Правила обращения с секретами:

- raw `access_token` / `refresh_token` / `id_token` / API keys — только в vault;
- секреты передаются адаптеру через `set_settings` по stdin, не через environment
  variables процесса;
- не отдавать raw secrets в event logs, обычные logs, chat и agent prompts;
- для display/debug — только redacted fingerprints/статусы.

Текущее хранилище — plaintext JSON на диске (режим full-trust/YOLO). Миграция на
OS keychain / encrypted vault — отдельная будущая задача; контракт адаптеров при
этом не меняется.

## Chat runtime

Отправка сообщения идёт через Core-managed run, event-driven:

```text
UI send_chat_message
  -> Tauri command (фоновый поток)
  -> Core begin_chat_run (user + pending assistant message в SQLite)
  -> ChatRunService.run
  -> Core-owned agentic loop
  -> SubprocessChatGateway -> adapter (chat_start, поток delta, chat_round_complete)
  -> Core executes any returned tool_calls and starts the next round
  -> SQLite deltas/status
  -> chat-run-event -> тонкая подписка UI
```

Для self-managed agent runtime flow короче:

```text
UI send_chat_message
  -> Core begin_chat_run
  -> ChatRunService.run
  -> SubprocessChatGateway -> adapter (single chat_start)
  -> adapter launches upstream headless agent process
  -> adapter streams delta and returns opaque state
  -> Core saves provider state on the chat
  -> SQLite deltas/status
  -> chat-run-event
```

Core не передаёт Mothership tool catalog и не запускает `AgenticLoopPolicy` для
`agent.runtime` adapters. Это осознанная граница: такие провайдеры уже имеют
собственный agent loop.

`ChatRunService` (`run.rs`):

1. читает выбранную модель; если не выбрана — run падает с понятной ошибкой
   («no LLM model selected; install a provider adapter and choose a model first»);
2. сканирует `AdapterRegistry` в `<data>/plugins` и находит адаптер по
   `provider_id` модели (иначе — «no adapter installed for provider: …»);
3. собирает контекст чата и гонит provider rounds через `SubprocessChatGateway`;
4. сам решает agentic loop: если адаптер вернул `tool_calls`, Core запускает
   batch через `ToolSupervisor`, сохраняет порядок результатов и передаёт их в
   следующий `chat_start`;
5. после broad safety budget Core делает final no-tool synthesis, а не адаптер.

`SubprocessChatGateway` (`subprocess_gateway.rs`) на каждый вызов: спавнит
процесс → вешает `store_secret_handler` (merge в vault) → `initialize` → грузит и
пушит настройки через `set_settings` → `transport_selected(Subprocess)` →
строит сообщения/continuation → `chat_start`, стримит `delta`, завершает на
`chat_round_complete`.

Важно: с точки зрения Core транспорт всегда `subprocess`
(`LlmTransportKind::Subprocess`). Реальный транспорт (HTTP/WS/SSE) и выбор
fallback — внутреннее дело адаптера, Core их не видит. Core владеет runtime
prompt/tool catalog/agentic loop; адаптер только мапит generic round request в
валидный для своего провайдера payload (для Codex — top-level `instructions`;
для OpenRouter — обычный массив `messages`) и возвращает opaque continuation
state.
Ошибки provider/model/auth/transport не замалчиваются — run становится `failed`,
UI получает ошибку.

## Tauri commands

Host тонкий и provider-agnostic. Актуальные команды, связанные с провайдерами
(`src-tauri/src/commands.rs`):

```text
get_connector_settings                   снимок: providers + selected_model
set_selected_model(provider_id, model)   валидирует по моделям установленных адаптеров
save_adapter_settings(provider_id, vals) merge настроек в общий vault (адаптер должен быть в реестре)
send_chat_message(chat_id, content)      begin_chat_run + фоновый ChatRunService
```

Провайдеры в снимке — это ровно установленные адаптеры: те, что отдают модели,
и те, что отдают форму настроек (свежеустановленный адаптер виден по форме
настроек ещё до того, как у него есть модели). `ConnectorProviderSummary`
содержит `{ id, label, settings_schema, models, selected_model_id, auth_kind,
auth_status, runtime_status, adapter_settings }`. `adapter_settings` —
задекларированные адаптером поля плюс текущие значения из vault, чтобы UI
отрисовал и сохранил конфиг-форму.

Старый host-driven Connect/OAuth flow удалён: команд `start_provider_auth` /
`complete_provider_auth` / `disconnect_provider_connection` /
`start_provider_oauth_login`, локального callback-листенера на `:1455` и
Connect/Disconnect UI больше нет. Адаптеры авторизуются сами.

## Реализованные адаптеры

### Codex (`adapters/codex`)

Построен на общем SDK (`mothership-adapter-sdk` — stdio-рантайм + транспорты) и
`mothership-openai-responses` (Responses-формы + фолбэк + парсинг); сам владеет
только провайдер-спецификой (OAuth, модели, request-wiring). Никогда не зависит
от `mothership-core`. Async (tokio). Auth — ленивый: первый чат без валидного
credential открывает браузер для логина.

- provider id: `codex`; auth schema: `oauth_internal`;
- модели: `ModelManagement::Server`;
- OAuth: PKCE S256, `client_id = app_EMoamEEZ73f0CkXaXp7hrann`,
  issuer `https://auth.openai.com`, scope `openid profile email offline_access`,
  redirect `http://localhost:1455/auth/callback` (листенер на `127.0.0.1:1455`);
- token endpoint: `https://auth.openai.com/oauth/token`; refresh — по
  приближению `expires_at` (запас 60 c);
- account id извлекается из claims JWT (`chatgpt_account_id`);
- модели: `GET https://chatgpt.com/backend-api/codex/models?client_version=0.133.0`
  с `Authorization: Bearer` и `ChatGPT-Account-Id`. `get_models` **никогда** не
  открывает браузер: без credential возвращается пустой список; список кэшируется
  на 5 минут;
- chat: **WS-primary → SSE → HTTP-JSON фолбэк** к `…/codex/responses`. Сначала
  persistent Responses-WebSocket (`wss://`, beta `responses_websockets=2026-02-06`,
  ленивый коннект, переиспользование между ходами, idle-close, per-process); при
  до-`committed` сбое — фолбэк на HTTP-SSE, затем нестриминговый HTTP-JSON; после
  первого токена («committed») сбой не ретраится. **Структурный** парсинг событий
  по `type` (`response.output_text.delta` → ответ; `response.reasoning_*` → НЕ
  подмешивается в ответ; `response.completed`/`failed`). Idle-таймауты на чтении
  (SSE 45 c, WS-кадр 20 c). Дефолтный system prompt — богатый (если кор не прислал
  свой);
- credential **не** хранится рядом с адаптером: host пушит его через
  `set_settings` под ключом `credential`, а адаптер возвращает выпущенный/
  обновлённый токен через `StoreSecret` в общий vault.

Logout/revoke: на `logout` адаптер делает best-effort server-side revoke токена
(`POST https://auth.openai.com/oauth/revoke`, предпочитая refresh-токен; форма
запроса как в upstream `auth/revoke.rs`), затем host забывает credential в vault.
Ошибка revoke не блокирует logout.

### OpenRouter (`adapters/openrouter`)

OpenAI-совместимый HTTP-провайдер.

- provider id: `openrouter`; auth schema: `api_key`;
- настройки: `api_key` (secret, required), `base_url` (text, optional, default
  `https://openrouter.ai/api/v1`), `models` (string_list, optional — список с +/−);
- модели: `ModelManagement::UserDefined` — список из поля `models`, первый
  помечается recommended;
- chat: SSE `POST {base}/chat/completions` с `Authorization: Bearer <api_key>`,
  парсинг `choices[0].delta.content`.

## Anthropic adapter (планируемый)

Anthropic-адаптера в коде ещё нет. Когда он появится, это будет такой же
subprocess-адаптер, как Codex (самодостаточный, владеет своим транспортом),
с auth-схемой `oauth_token_paste`: пользователь сам получает OAuth-токен любым
доступным ему способом и вставляет его; Mothership владеет refresh lifecycle
после этого.

Это явный architectural choice, а не временная заглушка. Рационал (важно
сохранить):

- Anthropic Consumer ToS (обновление от февраля 2026) прямо запрещает
  использование OAuth-токенов (`sk-ant-oat01-*`) в любом third-party приложении,
  «including the Agent SDK». Это юридическое/политическое, а не техническое
  ограничение.
- У Anthropic нет публичной OAuth client registration для third-party
  приложений, поэтому корректный browser-flow от имени Mothership реализовать
  нельзя.
- Единственный работающий `client_id` — собственный у Claude Code. Его
  переиспользование — это то, что сделал OpenCode; в результате они получили
  юридическое письмо и удалили пакет из npm. Mothership так не делает.
- Технически OAuth-токены **работают** в third-party приложении, если запрос
  идёт через `@anthropic-ai/claude-agent-sdk` с
  `systemPrompt: { type: 'preset', preset: 'claude_code' }`. С raw
  `@anthropic-ai/sdk` или кастомным system prompt Anthropic вернёт ошибку «This
  credential is only authorized for use with Claude Code». Этот preset — не
  опция, а единственное, что проводит OAuth-токены через серверный гейт
  Anthropic.
- Разделение ответственности: как пользователь получил токен — его дело и его
  риск перед ToS. Mothership не запускает `claude` CLI, не читает `~/.claude`, не
  reverse-engineer-ит OAuth flow и не переиспользует `client_id` Claude Code. Он
  принимает вставленный токен и владеет refresh с этого момента.

Жёсткие требования к будущему адаптеру:

- нет зависимости от Claude Code в runtime; нет запуска `claude` CLI; нет чтения
  `~/.claude`; нет scraping credentials Claude Code; Claude Code не владеет
  auth-состоянием Mothership;
- subscription auth не рекламируется как фича продукта в UI и маркетинге — это то,
  что выводит проекты на юридический радар Anthropic.

Ожидаемые технические детали:

- paste-input: `access_token`, `refresh_token`, `expires_at`, опционально
  `account_label`;
- refresh: `POST https://platform.claude.com/v1/oauth/token` (раньше был
  `console.anthropic.com`); access token живёт ~8 часов;
- profile/validation: `https://api.anthropic.com/api/oauth/profile`;
- LLM-транспорт: `@anthropic-ai/claude-agent-sdk` с `preset: 'claude_code'`;
- явно сообщать failure при revoked refresh token — Anthropic отзывает refresh
  tokens, если детектит third-party usage (UI просит пользователя повторно
  вставить токен).

Полноценный `anthropic_oauth_browser` откладывается до момента, когда Anthropic
откроет public OAuth client registration; тогда он добавляется **рядом** с
paste-адаптером, не вместо него.

## Что в Core от auth

Core не содержит provider-specific авторизации. Реальная авторизация целиком
внутри адаптеров (Codex водит свой browser OAuth; OpenRouter использует api key).
Core держит общий `FileCredentialVault` (`crates/mothership-core/src/auth/`) и
provider-agnostic `AuthStatus`, который адаптер возвращает через протокол.
Core не парсит provider token payload и не знает OAuth endpoints.

`ProviderAuthService`, connections/sessions/auth-methods, mock-адаптер,
`ProviderAuthAdapter`/registry, трейт `CredentialVault` и SQLite-таблицы
`auth_sessions`/`provider_connections`/`credential_records` **удалены целиком** —
это был пережиток дореформенной модели «Core владеет провайдерами». Sidecar
(`src-sidecar/src/main.rs`) тоже лишился всех `auth *` команд, осталась одна:

```powershell
cargo run -p mothership-sidecar -- status --database <db>
```

## Открытые вопросы и дальнейшее

- миграция vault с plaintext JSON на OS keychain / encrypted storage;
- резидентные инстансы/пул адаптеров вместо спавна на каждую операцию;
- distribution/lifecycle адаптеров: per-OS бинари, manifest + версионирование
  протокола, подписанный реестр («store»), health checks и crash backoff;
- `anthropic_oauth_browser`, если Anthropic откроет публичную регистрацию OAuth
  client'а.

Детали LLM-runtime — в [Architecture](ARCHITECTURE.md). Рационал по Anthropic
дополнительно зафиксирован в memory-заметке `mothership-provider-auth-context`.
