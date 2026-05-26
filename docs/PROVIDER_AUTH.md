# Provider Auth

Этот документ фиксирует направление ProviderAuth для Mothership.

Основано на обсуждении и ресерче проектов:

- `projects-to-research/opencode`;
- `projects-to-research/nanoclaw`;
- `projects-to-research/pi-agents`.

## Решение

В Mothership должен быть один универсальный Core для авторизации провайдеров.

Core не должен быть завязан на OpenAI, Anthropic, OAuth, API keys или любой
конкретный способ входа. OAuth - первый способ авторизации, который мы
реализуем, но не основная абстракция.

Основная абстракция:

```text
ProviderAuthCore
  -> provider
  -> auth method
  -> auth session
  -> provider connection
  -> credential reference
  -> provider gateway
```

Конкретика провайдеров живет в адаптерах:

```text
ProviderAuthAdapters
  openai_codex_oauth
  anthropic_oauth_token_paste
  future: openai_api_key
  future: anthropic_api_key
  future: anthropic_oauth_browser
  future: github_copilot_oauth
  future: openrouter_api_key
  future: other providers
```

## Требования продукта

Эти требования зафиксированы явно. Их нельзя ослаблять без отдельного
продуктового решения.

- Mothership должен работать как самостоятельное приложение.
- Mothership не должен зависеть от Claude Code для Anthropic authorization.
- Mothership не должен читать credentials Claude Code или состояние
  `~/.claude`.
- OpenAI subscription access должен быть реализован через Codex OAuth.
- Anthropic subscription access реализуется через manual paste OAuth-токена.
  Mothership не запускает собственный OAuth flow для Anthropic, так как
  Anthropic не предоставляет публичной OAuth client registration для
  third-party приложений. Пользователь самостоятельно получает токен и
  вставляет его; Mothership владеет refresh lifecycle после этого.
- Полноценный Anthropic OAuth (`anthropic_oauth_browser`) откладывается до
  момента, когда Anthropic откроет public client registration или будет
  принято отдельное продуктовое решение.
- Core должен быть одинаковым для OpenAI, Anthropic, API keys и будущих
  провайдеров.
- OAuth adapters являются provider-specific; Core не является OAuth-specific.
- API-key auth должен добавляться позже без изменения Core-концепций.
- Другие провайдеры должны добавляться позже без изменения Core-концепций.
- Сырые токены не должны попадать в UI state, chat messages, event logs,
  agent prompts, sidecar environment variables или обычные logs.
- Агент/runtime должен использовать provider connections через Core, а не
  сырые credentials.

## Цели

- Дать единую модель provider connection для desktop и будущих phone/web
  клиентов.
- Держать UI тонким: показывать auth status, запускать auth commands,
  завершать callbacks.
- Держать provider behavior в adapters, а не в Solid components или Tauri
  command handlers.
- Держать секреты за портом `CredentialVault`.
- Дать LLM runtime доступ к провайдерам через `ProviderGateway`.
- Поддержать browser OAuth и headless/device OAuth там, где adapter может это
  реализовать.
- Единообразно поддержать refresh, validation, disconnect и будущий revoke.
- Сохранить будущую поддержку API keys и non-OAuth providers.

## Не цели

- Не строить Anthropic auth path вокруг Claude Code.
- Не scrape/import credentials Claude Code.
- Не делать `OAuthCore` центральной абстракцией.
- Не передавать `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, access tokens или
  refresh tokens в agent sandboxes.
- Не размещать provider-specific условия в chat, runs, tools или UI state.
- Не использовать plaintext `auth.json` как финальную модель хранения
  секретов.
- Не использовать закрытые provider enum-ы, которые заставят менять Core под
  каждого нового провайдера.

## Выводы из ресерча

### opencode

`opencode` содержит самый полезный рабочий пример OpenAI/Codex OAuth.

Что стоит вытащить концептуально:

- генерация PKCE;
- browser OAuth flow;
- headless/device OAuth flow;
- local callback server pattern;
- authorization code exchange;
- refresh token flow;
- извлечение account id из token claims;
- rewrite запросов в Codex backend;
- injection `Authorization: Bearer <access_token>`;
- обработка `ChatGPT-Account-Id`;
- форма provider auth hook: methods, prompts, authorize, callback, loader.

Что не стоит копировать напрямую:

- plugin-coupled lifecycle;
- текущую credential file модель как финальное хранилище;
- runtime loader как есть - его нужно превратить в Mothership adapter и
  gateway boundary.

### nanoclaw

`nanoclaw` полезен прежде всего моделью изоляции credentials, а не готовым
standalone Anthropic OAuth flow.

Что стоит вытащить концептуально:

- сырые credentials не входят в agent containers;
- agent runtime видит placeholders или handles, а не реальные tokens;
- credential injection происходит на HTTP/gateway boundary;
- возможна per-agent credential policy;
- auth failures обрабатываются на host/core уровне.

Что не стоит копировать напрямую:

- не делать OneCLI обязательным vault для Mothership;
- не зависеть от Claude Code для получения Anthropic subscription tokens;
- не отдавать внешнему CLI владение auth state Mothership.

### pi-agents

`pi-agents` не владеет OAuth.

Он полезен как напоминание: orchestration layer должен потреблять уже
настроенный provider auth от host application. Он не должен владеть provider
login, token refresh или secret storage.

## Границы Core

ProviderAuth находится в Core.

Предлагаемая форма:

```text
crates/mothership-core/src/auth/
  domain.rs
  service.rs
  ports.rs
  repository.rs
  events.rs
  adapters/
    openai_codex_oauth.rs
    anthropic_oauth.rs
```

Точная раскладка файлов может измениться, но разделение ответственности должно
сохраниться.

Core владеет:

- lifecycle provider account;
- lifecycle auth session;
- metadata provider connection;
- credential references;
- выбором и хранением selected model;
- выбором default account;
- refresh state;
- auth status;
- auth events;
- redaction policy.

Adapters владеют:

- OAuth endpoints провайдера;
- PKCE details;
- browser/device/headless mechanics;
- token exchange;
- token refresh;
- token revoke, если provider это поддерживает;
- provider-owned model catalog или policy модели;
- provider settings schema для UI;
- provider account discovery;
- provider-specific metadata;
- provider request signing или rewriting.

Infrastructure владеет:

- SQLite repository implementation;
- OS credential store / vault implementation;
- local callback HTTP server;
- Tauri command adapter;
- opener/browser integration.

## Модель данных Core

SQLite должна хранить metadata, а не сырые secrets.

Предлагаемые таблицы:

```text
provider_accounts
  id
  provider_id
  auth_method_id
  external_account_id
  label
  email
  organization_id
  workspace_id
  scopes_json
  capabilities_json
  status
  is_default
  provider_metadata_json
  created_at
  updated_at
  last_used_at

credential_records
  id
  account_id
  credential_kind
  vault_handle
  expires_at
  refresh_expires_at
  fingerprint_hash
  status
  provider_metadata_json
  created_at
  updated_at

auth_sessions
  id
  provider_id
  auth_method_id
  mode
  state
  redirect_uri
  pkce_vault_handle
  status
  expires_at
  provider_metadata_json
  created_at
  updated_at
```

Для providers и auth methods использовать string ids:

```text
provider_id = "openai" | "anthropic" | "openrouter" | ...
auth_method_id = "codex_oauth_browser" | "oauth_device" | "api_key" | ...
```

Не нужно заставлять каждый будущий provider проходить через Rust enum, который
потребует менять Core API или миграции.

## Типы Core

Концептуальная модель:

```text
ProviderConnection
  id
  provider_id
  auth_method_id
  status
  account_label
  account_email
  scopes
  capabilities
  expires_at
  credential_ref
  provider_metadata

AuthMethod
  id
  provider_id
  kind
  label
  prompts
  capabilities

AuthSession
  id
  provider_id
  auth_method_id
  mode
  status
  authorization_url
  user_code
  expires_at

CredentialRef
  record_id
  vault_handle
```

Предлагаемые auth method kinds:

```text
oauth_browser
oauth_device
oauth_token_paste
api_key
manual_code
custom
```

Это формы методов, а не identity провайдеров.

`oauth_token_paste` — это форма, при которой пользователь сам получает OAuth
token set (любым способом, который ему доступен) и вставляет его в Mothership.
Mothership не запускает свой OAuth flow в этом методе, но владеет refresh,
validation, disconnect lifecycle после получения токена.

## Контракт адаптера

Adapters должны реализовывать общий provider auth contract примерно такого
вида:

```rust
trait ProviderAuthAdapter {
    fn provider_id(&self) -> ProviderId;
    fn auth_methods(&self) -> Vec<AuthMethod>;

    async fn start_auth(&self, input: StartAuthInput) -> Result<AuthStartResult>;
    async fn complete_auth(&self, input: CompleteAuthInput) -> Result<AuthCompleteResult>;
    async fn refresh(&self, input: RefreshCredentialInput) -> Result<RefreshCredentialResult>;
    async fn validate(&self, input: ValidateConnectionInput) -> Result<AuthStatus>;
    async fn disconnect(&self, input: DisconnectInput) -> Result<()>;
}
```

Выполнение provider requests держать отдельно:

```rust
trait ProviderGateway {
    async fn send(
        &self,
        connection_id: ProviderConnectionId,
        request: ProviderRequest,
    ) -> Result<ProviderResponse>;
}
```

Это разделение важно:

- `ProviderAuthAdapter` отвечает за login, callback, refresh, validation,
  metadata.
- `ProviderGateway` inject-ит credentials и выполняет provider-specific request
  rewrite.
- Chat/runs/tools зависят от provider gateway и connection ids, а не от OAuth
  tokens.

## Контракт моделей и настроек коннектора

Модели не должны жить одним глобальным hardcoded списком в UI.

Каждый LLM connector должен владеть своей логикой моделей:

```rust
trait LlmConnectorAdapter {
    fn provider_id(&self) -> &'static str;
    fn provider_label(&self) -> &'static str;
    fn bundled_models(&self) -> Vec<LlmModel>;
    fn remote_model_catalog(&self, input: RemoteModelCatalogInput) -> Result<Option<RemoteModelCatalog>>;
    fn settings_schema(&self) -> ConnectorSettingsSchema;
}
```

Core собирает доступные модели через registry адаптеров. UI получает уже
готовый snapshot и декларативную схему настроек, а не provider-specific код.

Первичные виды model management:

```text
fixed_catalog
remote_catalog
editable_list
```

- `fixed_catalog` — bundled/provider-owned список, который не требует сети.
- `remote_catalog` — provider-owned каталог, который забирается через endpoint
  провайдера и кэшируется локально на короткий TTL. Для OpenAI Codex OAuth это
  единственный источник моделей после подключения.
- `editable_list` — пользователь управляет списком моделей внутри коннектора.
  Это ожидаемый режим для OpenRouter/OpenAI-compatible providers, где нельзя
  показывать все модели провайдера и нужно дать пользователю добавить только
  нужные model ids.

Solid UI должен быть тонким рендерером schema: auth methods, connection status,
fixed model catalog, editable model list и future provider-specific fields вроде
base URL, headers или organization id.

Provider-specific правила отображения не должны расползаться по компонентам
настроек. Если OpenRouter требует list + add/remove model id, это описывает
OpenRouter adapter через schema. Если Codex не требует ручного добавления
моделей, его schema остается `remote_catalog`.

Для Codex нельзя считать bundled/fixed список source of truth. В официальном
Codex `model/list` опирается на remote catalog из ChatGPT/Codex backend:
`/codex/models` получает `client_version`, возвращает `models` и может вернуть
`ETag`; runtime хранит cache с коротким TTL и обновляет его при mismatch
`X-Models-Etag` на responses. В Mothership это правило такое:

- source of truth для подключенного Codex account — remote endpoint
  `https://chatgpt.com/backend-api/codex/models?client_version=...`;
- `client_version` должен соответствовать совместимой версии Codex client. На
  2026-05-22 актуальный `@openai/codex` в npm — `0.133.0`; Mothership держит
  это как default value и позволяет заменить через build env
  `MOTHERSHIP_CODEX_CLIENT_VERSION`;
- запрос выполняется только в Core, с `Authorization: Bearer <access_token>` и
  `ChatGPT-Account-Id`, если account id известен;
- raw token не выходит в UI;
- ответ нормализуется в общий `LlmModel`;
- результат сохраняется в `llm_model_catalog_cache` в SQLite;
- при 401 adapter один раз refresh-ит OAuth token через vault и повторяет
  запрос;
- без активной авторизации Codex models не показываются;
- при ошибке сети, endpoint, refresh или парсинга Core возвращает ошибку в UI;
- stale cache и bundled list не используются для имитации рабочего состояния,
  потому что если каталог моделей недоступен, запросы к модели с высокой
  вероятностью тоже не будут работать.

## Граница Credential Vault

Core должен определить порт `CredentialVault`.

Desktop infrastructure должна реализовать его через OS credential storage или
локальный encrypted vault. SQLite хранит только opaque handles и redacted
metadata.

Правила:

- Raw `access_token`, `refresh_token`, `id_token` и API keys хранить только в
  vault.
- Не отдавать raw secrets в UI responses.
- Не сохранять raw secrets в event logs.
- Не отправлять raw secrets в sidecar или agent environments.
- Не логировать raw auth headers в обычных logs.
- Для display/debug хранить только redacted fingerprints.

Credential payload внутри vault может быть provider-specific:

```json
{
  "type": "oauth_token_set",
  "access_token": "...",
  "refresh_token": "...",
  "id_token": "...",
  "expires_at": 1234567890,
  "provider_metadata": {}
}
```

Core должен считать payload в vault данными, которыми владеет adapter.

## Tauri/API Contract

Tauri commands должны оставаться тонкими и маршрутизировать запросы в Core.

Предлагаемые commands:

```text
auth_list_providers
auth_list_methods(provider_id)
auth_start(provider_id, auth_method_id, input)
auth_complete(auth_session_id, payload)
auth_list_connections
auth_get_connection(connection_id)
auth_set_default(provider_id, connection_id)
auth_refresh(connection_id)
auth_disconnect(connection_id)
```

Предлагаемые events:

```text
auth.session_started
auth.session_completed
auth.session_failed
auth.connection_created
auth.connection_updated
auth.connection_disconnected
auth.refresh_started
auth.refresh_completed
auth.refresh_failed
```

Phone и desktop clients должны использовать один и тот же command/query/event
contract.

## LLM Runtime Contract

Чат не должен работать через UI-заглушки или provider-specific код в Solid
components. Отправка сообщения идет через Core-managed run:

```text
UI command send_chat_message
  -> Tauri command adapter
  -> Core begin_chat_run
  -> SQLite user message + pending assistant message
  -> background run
  -> provider gateway
  -> SQLite deltas/status
  -> chat-run-event
  -> thin UI subscription
```

Runtime обязан быть event-driven:

- `send_chat_message` быстро возвращает `run_id`, user message и pending
  assistant message;
- дальнейшее выполнение идет в background run, не блокируя ввод и навигацию;
- Core пишет deltas/status в SQLite;
- Tauri публикует `chat-run-event`;
- UI только применяет snapshot события к текущему состоянию.

Транспорт LLM gateway выбирается строго в таком порядке:

```text
1. WebSocket
2. HTTP streaming/SSE
3. HTTP JSON без streaming
```

Fallback разрешен только до того, как transport начал отдавать текст. Если
WebSocket или SSE уже прислал часть ответа и потом сломался, run должен стать
`failed`, а не тихо переключаться на следующий transport поверх частичного
текста. Иначе можно получить дубли, перемешанный ответ и ложное ощущение, что
запрос завершился корректно.

Transport I/O должен быть bounded. WebSocket не может бесконечно ждать read;
HTTP streaming не может бесконечно держать pending response без завершения;
HTTP JSON имеет отдельный request timeout. Если процесс был остановлен или
упал с pending assistant message, startup recovery должен пометить такие
сообщения как `failed`, чтобы UI не оставался навсегда в `Thinking...`.

Ошибки provider/model/auth/transport нельзя замалчивать. Если remote catalog,
auth refresh, transport или response parsing не сработали, Core возвращает
ошибку в run/UI. Bundled model list или stale cache не используются как
имитация рабочего состояния для подключенного Codex account.

События runtime:

```text
started
transport_selected
delta
completed
failed
```

Event payload не должен содержать raw credentials, authorization headers,
refresh tokens, access tokens или provider secrets. Gateway получает secrets
только через `CredentialVault` и inject-ит их на HTTP/WebSocket boundary.

Для OpenAI Codex OAuth system prompt передается как top-level
`instructions` в Responses payload, а не как `role: "system"` message. Codex
backend отклоняет request без `instructions` ошибкой уровня `400 Bad Request`.
Но это provider-specific форма. Общий Core request должен оперировать
нейтральными понятиями `system_prompt` и `messages`; Codex connector/gateway
сам мапит `system_prompt` в `instructions` для всех transport payloads:
WebSocket, HTTP SSE и HTTP JSON. Будущий Anthropic/OpenRouter/API-key adapter
должен мапить тот же generic Core request в свой валидный provider payload.

## OpenAI Codex OAuth Adapter

Первый OpenAI OAuth adapter должен быть `openai_codex_oauth`.

Он должен быть standalone внутри Mothership и опираться на рабочий pattern из
`opencode`.

Ожидаемые обязанности:

- создать PKCE verifier/challenge;
- сгенерировать state;
- запустить local callback server для browser mode;
- поддержать headless/device mode, если доступно;
- обменять authorization code на tokens;
- сохранить token set в `CredentialVault`;
- извлечь account id из claims, если доступно;
- refresh-ить access token до истечения срока;
- обновлять vault и metadata после refresh;
- получать remote model catalog из Codex backend `/models` и кэшировать его;
- настроить provider gateway request rewrite для Codex backend;
- inject-ить `Authorization: Bearer <access_token>`;
- inject-ить `ChatGPT-Account-Id`, когда нужно.

Остальной Mothership не должен знать, как работает Codex OAuth.

## Anthropic Adapter

Первый Anthropic adapter — `anthropic_oauth_token_paste`.

Anthropic не предоставляет публичной регистрации OAuth client'а для third-party
приложений. Browser/device OAuth flow от имени Mothership реализовать легально
нельзя. Поэтому v1 идёт через manual paste: пользователь самостоятельно
получает OAuth token (через `claude login`, `claude setup-token` или иным
способом, который он сочтёт уместным) и вставляет access_token, refresh_token,
expires_at в Mothership. После этого Mothership владеет refresh lifecycle
полностью.

Это явный architectural choice, а не временная заглушка. Полноценный OAuth
adapter (`anthropic_oauth_browser`) откладывается до момента, когда Anthropic
откроет public OAuth client registration.

Жесткие требования:

- нет зависимости от Claude Code в runtime;
- нет запуска `claude` CLI из Mothership;
- нет чтения `~/.claude` или другого state Claude Code;
- нет scraping credentials Claude Code;
- нет автоматического OAuth flow от имени Mothership с переиспользованием
  `client_id` Claude Code;
- Claude Code не является владельцем auth state Mothership.

Как пользователь получил токен — его дело и его ответственность перед
Anthropic Terms of Service. Mothership не помогает с обходом ToS и не
рекламирует subscription auth как фичу продукта в UI и маркетинговых
материалах.

Ожидаемые обязанности адаптера:

- принимать paste-input (`access_token`, `refresh_token`, `expires_at`,
  опционально `account_label`);
- валидировать токен против `https://api.anthropic.com/api/oauth/profile`
  или эквивалентного endpoint;
- сохранять token set в `CredentialVault`;
- refresh-ить access_token через `https://platform.claude.com/v1/oauth/token`
  до истечения срока (8 часов для access_token);
- обновлять vault и metadata после refresh;
- discover-ить account metadata из profile endpoint, если доступно;
- явно сообщать failure при revoked refresh token — Anthropic отзывает
  refresh tokens, если детектит third-party usage.

Provider gateway для Anthropic должен использовать
`@anthropic-ai/claude-agent-sdk` с параметром
`systemPrompt: { type: 'preset', preset: 'claude_code' }`. Это технически
необходимо для совместимости с subscription OAuth токенами на серверной
стороне Anthropic. Использование raw `@anthropic-ai/sdk` с custom system
prompt + OAuth-токеном вернёт ошибку "This credential is only authorized for
use with Claude Code". Детали LLM runtime — в [Architecture](ARCHITECTURE.md),
модуль `llm/`.

## Будущая поддержка API keys

API keys добавляются как еще один auth method, а не как отдельный Core path.

Пример:

```text
provider_id = "anthropic"
auth_method_id = "api_key"
kind = "api_key"
```

`api_key` adapter:

- validates submitted key;
- сохраняет raw key в `CredentialVault`;
- создает или обновляет `ProviderConnection`;
- возвращает тот же auth status shape, что и OAuth.

LLM runtime не должен знать, пришло подключение из OAuth или API key.

## Порядок реализации

Рекомендуемый порядок:

1. Определить ProviderAuth domain types и events в Core.
2. Добавить SQLite metadata tables.
3. Добавить порт `CredentialVault`.
4. Добавить desktop vault implementation.
5. Добавить registry для `ProviderAuthAdapter`.
6. Добавить boundary `ProviderGateway`.
7. Добавить тонкие Tauri commands.
8. Добавить минимальный auth UI: status, connect, disconnect, default account.
9. Реализовать `openai_codex_oauth` по pattern из `opencode`.
10. Реализовать `anthropic_oauth_token_paste` adapter (paste-only, без своего
    OAuth flow); LLM gateway для Anthropic использует Claude Agent SDK с
    `preset: 'claude_code'`.
11. Добавить refresh scheduling и validation.
12. Добавить API-key adapters через тот же contract.
13. Подключить выбранные provider connections к LLM runtime и runs.
14. Добавить tests на redaction, vault, refresh и callback state.

## Текущий технический старт

Первый реализуемый срез делается как isolated console slice:

```text
mothership-sidecar auth ...
  -> mothership-core::auth
  -> ProviderAuthService
  -> ProviderAuthAdapterRegistry
  -> ProviderAuthRepository
  -> CredentialVault
```

Цель этого среза - проверить форму Core, metadata storage и secret boundary
без реального provider OAuth.

Текущие команды:

```powershell
cargo run -p mothership-sidecar -- auth providers --database tmp\provider-auth-lab.sqlite
cargo run -p mothership-sidecar -- auth connect-mock --database tmp\provider-auth-lab.sqlite --label ConsoleLab
cargo run -p mothership-sidecar -- auth connections --database tmp\provider-auth-lab.sqlite
cargo run -p mothership-sidecar -- auth disconnect --database tmp\provider-auth-lab.sqlite --connection <connection_id>
```

OpenAI Codex-only console commands:

```powershell
cargo run -p mothership-sidecar -- auth openai-start --database tmp\provider-auth-lab.sqlite
cargo run -p mothership-sidecar -- auth openai-complete --database tmp\provider-auth-lab.sqlite --session <session_id> --callback-url "<full_callback_url>"
cargo run -p mothership-sidecar -- auth openai-login --database tmp\provider-auth-lab.sqlite
```

`openai-start` создает auth session и печатает authorization URL. Это удобно
для отладки ручного callback.

`openai-complete` принимает полный callback URL с `code` и `state`, валидирует
state, меняет authorization code на Codex OAuth token set и сохраняет credential.

`openai-login` поднимает локальный callback listener на
`127.0.0.1:1455/auth/callback`, печатает URL для браузера, ожидает callback,
завершает OAuth flow и создает OpenAI provider connection.

Текущий `mock-ai` adapter нужен только для проверки lifecycle:

```text
start auth -> complete auth -> store secret via vault -> save connection metadata
```

Это не финальная auth реализация и не замена OpenAI/Anthropic adapters.

Текущий console slice использует `FileCredentialVault`, но не как один большой
`auth.json`. Каждый credential пишется в отдельный файл:

```text
<database-dir>/auth/credentials/<provider>_<timestamp>_<sequence>.json
```

Причины:

- один поврежденный credential-файл не ломает остальные авторизации;
- `auth connections` читает SQLite metadata и не зависит от чтения raw secrets;
- adapter-specific payload изолирован внутри одного credential-файла;
- запись идет через temp file + atomic rename, чтобы не оставлять частично
  записанный JSON при сбое процесса;
- `vault_handle` остается opaque reference для Core/UI.

Если новый adapter сломает сериализацию своего payload, ошибка должна
ограничиться конкретным connection при его использовании. Остальные
connections должны продолжать отображаться и использоваться.

`InMemoryCredentialVault` остается только для unit tests. Production desktop
storage позже можно заменить на OS credential storage/encrypted vault без
изменения contract adapters.

## OpenAI Codex OAuth текущая реализация

Текущий OpenAI path реализует только Codex OAuth:

- provider id: `openai`;
- auth method id: `codex_oauth_browser`;
- issuer: `https://auth.openai.com`;
- redirect URI: `http://localhost:1455/auth/callback`;
- PKCE: S256;
- scope: `openid profile email offline_access`;
- token endpoint: `https://auth.openai.com/oauth/token`;
- revoke endpoint: `https://auth.openai.com/oauth/revoke`;
- Codex backend rewrite:
  `https://chatgpt.com/backend-api/codex/responses`;
- gateway header injection:
  - `Authorization: Bearer <access_token>`;
  - `ChatGPT-Account-Id: <account_id>`, если account id найден в token claims.

OpenAI adapter хранит PKCE verifier и OAuth state только в private auth session
metadata. `AuthSession` serialization не отдает `provider_metadata` наружу.

ProviderGateway skeleton умеет:

- загрузить credential через `CredentialVault`;
- refresh-ить access token перед использованием, если credential истекает;
- заменить credential через `CredentialVault::replace`;
- подготовить Codex request URL и headers.

Disconnect для Codex должен быть best-effort:

1. загрузить credential из vault;
2. отправить refresh token на `https://auth.openai.com/oauth/revoke`:

```json
{
  "token": "<refresh_token>",
  "token_type_hint": "refresh_token",
  "client_id": "app_EMoamEEZ73f0CkXaXp7hrann"
}
```

3. если refresh token отсутствует, попробовать access token с
   `token_type_hint: "access_token"` без `client_id`;
4. независимо от результата revoke удалить local vault entry и пометить
   connection disconnected.

Такой порядок соответствует свежей реализации `openai/codex`: remote revoke не
должен блокировать локальный logout, потому что сеть или provider endpoint
могут быть недоступны.

## Требования безопасности

- Callback state должен валидироваться.
- PKCE verifier должен храниться только transient или в vault.
- Auth sessions должны истекать.
- Refresh должен coalesce-иться per credential, чтобы избежать багов
  concurrent rotation.
- Logs должны redact-ить authorization headers и token-like values.
- Event payloads не должны содержать raw credentials.
- UI responses должны включать только redacted metadata.
- Disconnect должен удалять local credential records и vault entries.
- Provider revocation должен поддерживаться там, где adapter может это
  реализовать.
- Raw токены нельзя просить у пользователя в чат. Для диагностики revoke нужно
  использовать локальный probe, который читает credential из vault и выводит
  только статус/ошибку без секретов.

## Открытые вопросы

- Anthropic не предоставляет публичной OAuth client registration для
  third-party; возможно ли (и нужно ли) запрашивать её отдельно для Mothership.
- Какой OS credential storage crate использовать первым?
- Где должен жить provider HTTP gateway в v1: Core, sidecar или Tauri host?
- Как future phone client должен безопасно запускать browser/device auth
  flows (для OpenAI Codex OAuth и будущих провайдеров, у которых публичный
  flow есть).
- Как корректно обрабатывать revoked refresh token у Anthropic (UI prompt
  пользователю на повторный paste).

Это вопросы adapter/infrastructure. Они не должны менять форму
ProviderAuth Core.
