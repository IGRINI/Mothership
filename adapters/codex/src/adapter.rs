use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, ImageGenerationRequest, ImageGenerationResult, Model, ModelManagement,
    ProviderService,
};
use mothership_adapter_sdk::ws::WsSession;
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome, Context, ProviderAdapter};
use mothership_openai_responses as responses;

use crate::auth::{self, CodexCredential, CREDENTIAL_SETTINGS_KEY};

pub(crate) struct CodexAdapter {
    client: reqwest::Client,
    endpoint: responses::Endpoint,
    credential: Option<CodexCredential>,
    /// Persistent backend WebSocket, lazily opened on first chat and reused
    /// across turns; dropped on idle / token change / WS failure.
    ws: Option<WsSession>,
    /// Set when a WS attempt fell back; skip WS until the credential changes.
    ws_disabled: bool,
}

impl CodexAdapter {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: mothership_adapter_sdk::http::client(Duration::from_secs(30)),
            endpoint: responses::Endpoint::from_https(crate::chat::RESPONSES_ENDPOINT)?,
            credential: None,
            ws: None,
            ws_disabled: false,
        })
    }

    /// Replace the credential and invalidate everything derived from it.
    fn set_credential(&mut self, credential: Option<CodexCredential>) {
        self.credential = credential;
        self.ws = None;
        self.ws_disabled = false;
    }

    /// A valid access token + account id, running browser OAuth if there is no
    /// credential and refreshing if near expiry. Persists anything minted.
    async fn ensure_token(&mut self, ctx: &Context) -> Result<(String, Option<String>)> {
        if self.credential.is_none() {
            let fresh = auth::run_oauth(&self.client).await?;
            auth::persist_credential(ctx, &fresh);
            self.set_credential(Some(fresh));
        }
        self.refresh_if_needed(ctx).await?;
        let credential = self
            .credential
            .as_ref()
            .expect("credential present after ensure");
        Ok((
            credential.access_token.clone(),
            credential.account_id.clone(),
        ))
    }

    async fn refresh_if_needed(&mut self, ctx: &Context) -> Result<()> {
        if auth::refresh_if_needed(&self.client, ctx, &mut self.credential).await? {
            self.ws = None;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for CodexAdapter {
    fn identity(&self) -> (String, String) {
        ("codex".to_string(), "Codex".to_string())
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::OauthInternal
    }

    fn auth_status(&self) -> AuthStatus {
        auth::credential_auth_status(self.credential.as_ref())
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        self.set_credential(
            values
                .get(CREDENTIAL_SETTINGS_KEY)
                .and_then(|raw| serde_json::from_str(raw).ok()),
        );
        Ok(())
    }

    async fn models(&mut self, ctx: &Context) -> Result<(ModelManagement, Vec<Model>)> {
        // Never trigger OAuth from model listing; only use an existing credential.
        let models = if self.credential.is_some() {
            self.refresh_if_needed(ctx).await?;
            let (access_token, account_id) = {
                let credential = self.credential.as_ref().expect("credential present");
                (
                    credential.access_token.clone(),
                    credential.account_id.clone(),
                )
            };
            crate::models::fetch_models(&self.client, &access_token, account_id.as_deref()).await?
        } else {
            Vec::new()
        };
        Ok((ModelManagement::Server, models))
    }

    async fn services(&mut self, ctx: &Context) -> Result<Vec<ProviderService>> {
        // Never trigger OAuth from service listing; only use an existing credential.
        if self.credential.is_none() {
            return Ok(Vec::new());
        }
        self.refresh_if_needed(ctx).await?;
        let (access_token, account_id) = {
            let credential = self.credential.as_ref().expect("credential present");
            (
                credential.access_token.clone(),
                credential.account_id.clone(),
            )
        };
        let models =
            crate::models::fetch_models(&self.client, &access_token, account_id.as_deref()).await?;
        Ok(crate::services::service_catalog_from_models(models))
    }

    async fn authenticate(&mut self, ctx: &Context) -> Result<()> {
        let fresh = auth::run_oauth(&self.client).await?;
        auth::persist_credential(ctx, &fresh);
        self.set_credential(Some(fresh));
        Ok(())
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        ctx: &Context,
        sink: &mut mothership_adapter_sdk::ChatSink,
    ) -> Result<ChatRoundOutcome> {
        let (access_token, account_id) = self.ensure_token(ctx).await?;
        crate::chat::run_chat_round(
            &self.client,
            &self.endpoint,
            &mut self.ws,
            &mut self.ws_disabled,
            &access_token,
            account_id.as_deref(),
            request,
            sink,
        )
        .await
    }

    async fn generate_image(
        &mut self,
        request: ImageGenerationRequest,
        ctx: &Context,
    ) -> Result<ImageGenerationResult> {
        let (access_token, account_id) = self.ensure_token(ctx).await?;
        crate::services::generate_image(
            &self.client,
            &self.endpoint,
            &access_token,
            account_id.as_deref(),
            request,
        )
        .await
    }

    async fn logout(&mut self, _ctx: &Context) -> Result<()> {
        if let Some(credential) = self.credential.as_ref() {
            auth::revoke_credential(&self.client, credential).await;
        }
        self.set_credential(None);
        Ok(())
    }

    async fn on_idle(&mut self, _ctx: &Context) {
        if let Some(session) = self.ws.as_mut() {
            session.close_if_idle().await;
        }
    }
}
