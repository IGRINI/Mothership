use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _};
use mothership_adapter_sdk::Context as AdapterContext;
use serde_json::Value;
use tokio::process::Command;

use crate::settings::{self, ClaudeAgentSettings, OAUTH_TOKEN_KEY};

const CREDENTIALS_FILE_NAME: &str = ".credentials.json";

pub(crate) struct ClaudeAuthRuntime {
    source: ClaudeAuthSource,
    temp_config: Option<TempClaudeConfig>,
}

enum ClaudeAuthSource {
    ConfigDir(PathBuf),
    EnvOauthToken(String),
}

struct TempClaudeConfig {
    path: PathBuf,
    original_credentials: String,
}

impl ClaudeAuthRuntime {
    pub(crate) fn prepare(settings: &ClaudeAgentSettings) -> anyhow::Result<Self> {
        let credential_payload = settings.credential_payload().trim();
        if !credential_payload.is_empty() {
            if let Some(credentials) = credentials_json(credential_payload)? {
                let temp_config = TempClaudeConfig::create(credentials)?;
                return Ok(Self {
                    source: ClaudeAuthSource::ConfigDir(temp_config.path.clone()),
                    temp_config: Some(temp_config),
                });
            }

            return Ok(Self {
                source: ClaudeAuthSource::EnvOauthToken(credential_payload.to_string()),
                temp_config: None,
            });
        }

        if let Some(path) = configured_config_dir(settings) {
            return Ok(Self {
                source: ClaudeAuthSource::ConfigDir(path),
                temp_config: None,
            });
        }

        bail!("Claude credentials are not configured");
    }

    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        match &self.source {
            ClaudeAuthSource::ConfigDir(path) => {
                command.env("CLAUDE_CONFIG_DIR", path);
                if cfg!(windows) {
                    command.env("CLAUDE_SECURESTORAGE_CONFIG_DIR", path);
                }
            }
            ClaudeAuthSource::EnvOauthToken(token) => {
                command.env("CLAUDE_CODE_OAUTH_TOKEN", token);
            }
        }
    }

    pub(crate) fn persist_refreshed_credentials(&self, ctx: Option<&AdapterContext>) {
        let (Some(ctx), Some(temp_config)) = (ctx, self.temp_config.as_ref()) else {
            return;
        };
        let Ok(credentials) = fs::read_to_string(temp_config.credentials_path()) else {
            return;
        };
        if credentials == temp_config.original_credentials {
            return;
        }
        ctx.store_secret(BTreeMap::from([(OAUTH_TOKEN_KEY.to_string(), credentials)]));
    }
}

impl Drop for TempClaudeConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl TempClaudeConfig {
    fn create(credentials: String) -> anyhow::Result<Self> {
        let path = create_temp_config_dir().context("create temporary Claude config directory")?;
        fs::write(path.join(CREDENTIALS_FILE_NAME), credentials.as_bytes())
            .context("write temporary Claude credentials")?;
        Ok(Self {
            path,
            original_credentials: credentials,
        })
    }

    fn credentials_path(&self) -> PathBuf {
        self.path.join(CREDENTIALS_FILE_NAME)
    }
}

fn configured_config_dir(settings: &ClaudeAgentSettings) -> Option<PathBuf> {
    settings
        .config_dir()
        .cloned()
        .filter(|path| path.join(CREDENTIALS_FILE_NAME).is_file())
        .or_else(|| {
            settings::default_claude_config_dir()
                .filter(|path| path.join(CREDENTIALS_FILE_NAME).is_file())
        })
}

fn credentials_json(payload: &str) -> anyhow::Result<Option<String>> {
    let payload = payload.trim();
    if payload.is_empty() {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return Ok(None);
    };

    let normalized = if value.get("claudeAiOauth").is_some() {
        value
    } else if value.get("accessToken").is_some() && value.get("refreshToken").is_some() {
        serde_json::json!({ "claudeAiOauth": value })
    } else {
        bail!("Claude credentials JSON must contain `claudeAiOauth.accessToken` and `claudeAiOauth.refreshToken`");
    };

    let oauth = normalized
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow::anyhow!("Claude credentials JSON must contain a `claudeAiOauth` object")
        })?;
    for key in ["accessToken", "refreshToken"] {
        if oauth
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            bail!("Claude credentials JSON is missing `claudeAiOauth.{key}`");
        }
    }

    serde_json::to_string(&normalized)
        .map(Some)
        .context("encode Claude credentials JSON")
}

fn create_temp_config_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir().join("mothership-claude-agent");
    fs::create_dir_all(&base)?;
    for attempt in 0..100_u32 {
        let candidate = base.join(format!("{}-{}-{attempt}", process::id(), timestamp_nanos()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary Claude config directory",
    ))
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_json_accepts_full_claude_credentials_file() {
        let credentials = credentials_json(
            r#"{"claudeAiOauth":{"accessToken":"access","refreshToken":"refresh"}}"#,
        )
        .expect("parse credentials")
        .expect("credentials");
        let value: Value = serde_json::from_str(&credentials).expect("json");

        assert_eq!(value["claudeAiOauth"]["accessToken"], "access");
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "refresh");
    }

    #[test]
    fn credentials_json_wraps_inner_oauth_object() {
        let credentials = credentials_json(r#"{"accessToken":"access","refreshToken":"refresh"}"#)
            .expect("parse credentials")
            .expect("credentials");
        let value: Value = serde_json::from_str(&credentials).expect("json");

        assert_eq!(value["claudeAiOauth"]["accessToken"], "access");
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "refresh");
    }

    #[test]
    fn credentials_json_treats_plain_token_as_env_token() {
        assert!(credentials_json("token").expect("plain token").is_none());
    }

    #[test]
    fn prepare_prefers_explicit_plain_token_over_config_dir() {
        let settings = ClaudeAgentSettings::from_values(BTreeMap::from([
            (OAUTH_TOKEN_KEY.to_string(), "token".to_string()),
            (
                settings::CONFIG_DIR_KEY.to_string(),
                "this-path-must-not-be-read".to_string(),
            ),
        ]));
        let runtime = ClaudeAuthRuntime::prepare(&settings).expect("runtime");

        match runtime.source {
            ClaudeAuthSource::EnvOauthToken(token) => assert_eq!(token, "token"),
            ClaudeAuthSource::ConfigDir(path) => {
                panic!(
                    "expected explicit token to win, got config dir {}",
                    path.display()
                )
            }
        }
    }

    #[test]
    fn credentials_json_rejects_refresh_token_without_access_token() {
        let error = credentials_json(r#"{"refreshToken":"refresh"}"#)
            .expect_err("invalid credentials must fail");

        assert!(error.to_string().contains("accessToken"));
    }
}
