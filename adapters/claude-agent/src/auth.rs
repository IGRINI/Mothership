//! Credential plumbing for the Claude CLI.
//!
//! When the user pastes a credentials JSON, the adapter materializes it as a
//! `.credentials.json` inside a PERSISTENT per-provider config dir (see
//! [`persistent_config_dir`]) and points the CLI at it via `CLAUDE_CONFIG_DIR`.
//! The dir must outlive a chat round: the CLI stores session transcripts under
//! its config dir, and the next round resumes with `--resume <session_id>` —
//! deleting the dir between rounds breaks every follow-up message. It must also
//! survive hard kills (the host terminates adapters without running
//! destructors), so cleanup never relies on `Drop`; stale dirs left by old
//! per-round temp-dir versions are swept at process start instead.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use mothership_adapter_sdk::Context as AdapterContext;
use serde_json::Value;
use tokio::process::Command;

use crate::settings::{self, ClaudeAgentSettings, OAUTH_TOKEN_KEY};

const CREDENTIALS_FILE_NAME: &str = ".credentials.json";
/// Base dir (under the OS temp dir) used by old adapter versions that created a
/// fresh config dir per chat round. Swept at startup; never written anymore.
const LEGACY_TEMP_DIR_NAME: &str = "mothership-claude-agent";

/// Per-adapter-process auth bookkeeping shared across chat rounds: remembers
/// the configured credentials payload that was last reconciled with the
/// persistent config dir, so an UNCHANGED secret never overwrites a token the
/// Claude CLI refreshed on disk in the meantime.
#[derive(Clone, Default)]
pub(crate) struct ClaudeAuthState {
    last_written_credentials: Arc<Mutex<Option<String>>>,
}

pub(crate) struct ClaudeAuthRuntime {
    source: ClaudeAuthSource,
    persistent_config: Option<PersistentClaudeConfig>,
}

enum ClaudeAuthSource {
    ConfigDir(PathBuf),
    EnvOauthToken(String),
}

/// The persistent, adapter-owned Claude config dir for pasted credentials.
/// Intentionally has no `Drop` cleanup: session transcripts in it are required
/// by `--resume` on later rounds, and the host may kill the process at any
/// time anyway.
struct PersistentClaudeConfig {
    path: PathBuf,
    /// What `.credentials.json` contained when the round started, so a CLI
    /// token refresh during the round can be detected and pushed back into the
    /// host vault.
    original_credentials: String,
}

impl ClaudeAuthRuntime {
    pub(crate) fn prepare(
        settings: &ClaudeAgentSettings,
        state: &ClaudeAuthState,
    ) -> anyhow::Result<Self> {
        Self::prepare_with_config_dir(settings, &persistent_config_dir(), state)
    }

    fn prepare_with_config_dir(
        settings: &ClaudeAgentSettings,
        config_dir: &Path,
        state: &ClaudeAuthState,
    ) -> anyhow::Result<Self> {
        let credential_payload = settings.credential_payload().trim();
        if !credential_payload.is_empty() {
            if let Some(credentials) = credentials_json(credential_payload)? {
                let persistent_config =
                    PersistentClaudeConfig::prepare_in(config_dir, credentials, state)?;
                return Ok(Self {
                    source: ClaudeAuthSource::ConfigDir(persistent_config.path.clone()),
                    persistent_config: Some(persistent_config),
                });
            }

            return Ok(Self {
                source: ClaudeAuthSource::EnvOauthToken(credential_payload.to_string()),
                persistent_config: None,
            });
        }

        if let Some(path) = configured_config_dir(settings) {
            return Ok(Self {
                source: ClaudeAuthSource::ConfigDir(path),
                persistent_config: None,
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
        let (Some(ctx), Some(persistent_config)) = (ctx, self.persistent_config.as_ref()) else {
            return;
        };
        let Ok(credentials) = fs::read_to_string(persistent_config.credentials_path()) else {
            return;
        };
        if credentials == persistent_config.original_credentials {
            return;
        }
        ctx.store_secret(BTreeMap::from([(OAUTH_TOKEN_KEY.to_string(), credentials)]));
    }
}

impl PersistentClaudeConfig {
    /// Reuses `config_dir` across rounds, refreshing `.credentials.json` only
    /// when needed:
    /// - file missing → write the configured payload;
    /// - configured secret unchanged since the last reconcile in this process
    ///   → leave the file alone (it may hold a token the CLI refreshed);
    /// - configured secret changed (or first round of this process) → write it,
    ///   unless the file already matches byte-for-byte.
    fn prepare_in(
        config_dir: &Path,
        credentials: String,
        state: &ClaudeAuthState,
    ) -> anyhow::Result<Self> {
        create_private_dir(config_dir).with_context(|| {
            format!(
                "create persistent Claude config directory {}",
                config_dir.display()
            )
        })?;
        let credentials_path = config_dir.join(CREDENTIALS_FILE_NAME);
        let mut last_written = state.last_written_credentials.lock().unwrap();
        let secret_unchanged = last_written.as_deref() == Some(credentials.as_str());

        let original_credentials = match fs::read_to_string(&credentials_path).ok() {
            Some(existing) if secret_unchanged || existing == credentials => existing,
            _ => {
                write_private_file(&credentials_path, credentials.as_bytes())
                    .context("write Claude credentials")?;
                credentials.clone()
            }
        };
        *last_written = Some(credentials);

        Ok(Self {
            path: config_dir.to_path_buf(),
            original_credentials,
        })
    }

    fn credentials_path(&self) -> PathBuf {
        self.path.join(CREDENTIALS_FILE_NAME)
    }
}

/// The stable config dir handed to the CLI via `CLAUDE_CONFIG_DIR` for pasted
/// credentials. Stable across rounds AND process restarts — `--resume` needs
/// the session transcripts the CLI stores under it.
fn persistent_config_dir() -> PathBuf {
    persistent_data_root().join("config")
}

/// Per-user app-data root for this adapter. The adapter protocol does not carry
/// a host-provided storage dir, so derive a conventional one: Windows
/// `%LOCALAPPDATA%\mothership\claude-agent`, Unix `$XDG_DATA_HOME` /
/// `~/.local/share`. Last resort is a STABLE path under the OS temp dir that is
/// deliberately distinct from the swept legacy base ([`LEGACY_TEMP_DIR_NAME`]).
fn persistent_data_root() -> PathBuf {
    let provider_root = |base: PathBuf| base.join("mothership").join("claude-agent");
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        return provider_root(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return provider_root(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return provider_root(PathBuf::from(home).join(".local").join("share"));
    }
    std::env::temp_dir().join("mothership-claude-agent-config")
}

/// Best-effort startup sweep of `%TEMP%/mothership-claude-agent/*`: old adapter
/// versions created one config dir (containing a plaintext `.credentials.json`)
/// per chat round there and relied on `Drop` to delete it — which never ran
/// when the host hard-killed the process, leaking credentials indefinitely.
pub(crate) fn sweep_legacy_temp_config_dirs() {
    sweep_legacy_temp_config_dirs_in(&std::env::temp_dir().join(LEGACY_TEMP_DIR_NAME));
}

fn sweep_legacy_temp_config_dirs_in(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false);
        let _ = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    let _ = fs::remove_dir(base);
}

/// True when the configured payload no longer carries a credentials JSON (the
/// secret was cleared, or replaced by a plain `claude setup-token` token), so
/// any `.credentials.json` previously persisted by this adapter is stale.
/// Malformed-but-credentials-shaped payloads keep the file: the user may be
/// mid-edit, and `prepare` refuses to run with them anyway.
pub(crate) fn persisted_credentials_are_stale(credential_payload: &str) -> bool {
    matches!(credentials_json(credential_payload), Ok(None))
}

/// Best-effort removal of the adapter-persisted `.credentials.json` (logout /
/// secret cleared). Leaves the rest of the config dir (session transcripts)
/// in place.
pub(crate) fn remove_persisted_credentials() {
    remove_persisted_credentials_in(&persistent_config_dir());
}

fn remove_persisted_credentials_in(config_dir: &Path) {
    let _ = fs::remove_file(config_dir.join(CREDENTIALS_FILE_NAME));
}

/// Creates the dir (and parents). On Unix the leaf is forced to `0700` so the
/// credentials inside are not world-readable.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        return fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)
}

/// Writes `contents` to `path`. On Unix the file is created with — and forced
/// to — mode `0600` BEFORE the secret bytes are written, so the credentials are
/// never readable by other users (plain `fs::write` would create `0644`).
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies when the file is created; tighten a pre-existing
        // file too, while it is still empty from the truncate above.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        return file.write_all(contents);
    }
    #[cfg(not(unix))]
    fs::write(path, contents)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "mothership-claude-agent-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn credentials_settings(access_token: &str) -> ClaudeAgentSettings {
        ClaudeAgentSettings::from_values(BTreeMap::from([(
            OAUTH_TOKEN_KEY.to_string(),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"{access_token}","refreshToken":"refresh"}}}}"#
            ),
        )]))
    }

    fn config_dir_of(runtime: &ClaudeAuthRuntime) -> PathBuf {
        match &runtime.source {
            ClaudeAuthSource::ConfigDir(path) => path.clone(),
            ClaudeAuthSource::EnvOauthToken(_) => panic!("expected config-dir auth source"),
        }
    }

    fn read_access_token(config_dir: &Path) -> String {
        let raw = fs::read_to_string(config_dir.join(CREDENTIALS_FILE_NAME))
            .expect("read persisted credentials");
        let value: Value = serde_json::from_str(&raw).expect("credentials json");
        value["claudeAiOauth"]["accessToken"]
            .as_str()
            .expect("accessToken")
            .to_string()
    }

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
        let runtime =
            ClaudeAuthRuntime::prepare(&settings, &ClaudeAuthState::default()).expect("runtime");

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

    #[test]
    fn config_dir_is_identical_across_consecutive_rounds_and_survives_drop() {
        let base = unique_test_dir("stable-dir");
        let state = ClaudeAuthState::default();
        let settings = credentials_settings("access");

        let first = ClaudeAuthRuntime::prepare_with_config_dir(&settings, &base, &state)
            .expect("first round");
        let first_dir = config_dir_of(&first);
        drop(first);

        // Multi-turn contract: dropping the round must NOT delete the config
        // dir — the CLI's session transcripts live there and the next round
        // resumes via `--resume`.
        assert!(first_dir.is_dir(), "config dir must survive end of round");
        assert!(
            first_dir.join(CREDENTIALS_FILE_NAME).is_file(),
            "credentials must survive end of round"
        );

        let second = ClaudeAuthRuntime::prepare_with_config_dir(&settings, &base, &state)
            .expect("second round");
        assert_eq!(
            first_dir,
            config_dir_of(&second),
            "both rounds must point the CLI at the same config dir"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn startup_sweep_removes_legacy_temp_dirs() {
        let base = unique_test_dir("sweep");
        let stale_round_dir = base.join("123-456789-0");
        fs::create_dir_all(&stale_round_dir).expect("create legacy round dir");
        fs::write(
            stale_round_dir.join(CREDENTIALS_FILE_NAME),
            b"{\"leaked\":1}",
        )
        .expect("write leaked credentials");
        fs::write(base.join("stray.txt"), b"stray").expect("write stray file");

        sweep_legacy_temp_config_dirs_in(&base);

        assert!(
            !base.exists(),
            "sweep must remove the whole legacy base dir"
        );
    }

    #[test]
    fn credentials_file_refreshes_only_when_configured_secret_changes() {
        let base = unique_test_dir("refresh");
        let state = ClaudeAuthState::default();

        ClaudeAuthRuntime::prepare_with_config_dir(&credentials_settings("one"), &base, &state)
            .expect("first round");
        assert_eq!(read_access_token(&base), "one");

        // Simulate the Claude CLI refreshing the token on disk mid-session.
        write_private_file(
            &base.join(CREDENTIALS_FILE_NAME),
            br#"{"claudeAiOauth":{"accessToken":"cli-refreshed","refreshToken":"rotated"}}"#,
        )
        .expect("simulate CLI refresh");

        // Same configured secret → the refreshed file must NOT be clobbered.
        ClaudeAuthRuntime::prepare_with_config_dir(&credentials_settings("one"), &base, &state)
            .expect("second round");
        assert_eq!(
            read_access_token(&base),
            "cli-refreshed",
            "unchanged secret must not overwrite a CLI-refreshed token"
        );

        // Changed configured secret → the file must be rewritten.
        ClaudeAuthRuntime::prepare_with_config_dir(&credentials_settings("two"), &base, &state)
            .expect("third round");
        assert_eq!(
            read_access_token(&base),
            "two",
            "a changed secret must refresh the persisted credentials"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_credentials_file_is_recreated_even_when_secret_unchanged() {
        let base = unique_test_dir("recreate");
        let state = ClaudeAuthState::default();
        let settings = credentials_settings("access");

        ClaudeAuthRuntime::prepare_with_config_dir(&settings, &base, &state).expect("first round");
        fs::remove_file(base.join(CREDENTIALS_FILE_NAME)).expect("delete credentials");

        ClaudeAuthRuntime::prepare_with_config_dir(&settings, &base, &state).expect("second round");
        assert_eq!(read_access_token(&base), "access");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_persisted_credentials_deletes_only_the_credentials_file() {
        let base = unique_test_dir("logout");
        let state = ClaudeAuthState::default();
        ClaudeAuthRuntime::prepare_with_config_dir(&credentials_settings("access"), &base, &state)
            .expect("prepare");
        fs::write(base.join("session.jsonl"), b"transcript").expect("write transcript");

        remove_persisted_credentials_in(&base);

        assert!(
            !base.join(CREDENTIALS_FILE_NAME).exists(),
            "credentials file must be removed"
        );
        assert!(
            base.join("session.jsonl").is_file(),
            "session transcripts must be left alone"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn persisted_credentials_staleness_follows_payload_kind() {
        // Cleared secret → stale.
        assert!(persisted_credentials_are_stale(""));
        // Plain setup-token → the JSON copy is stale.
        assert!(persisted_credentials_are_stale("sk-ant-token"));
        // Valid credentials JSON → keep.
        assert!(!persisted_credentials_are_stale(
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#
        ));
        // Malformed credentials-shaped JSON (user mid-edit) → keep.
        assert!(!persisted_credentials_are_stale(
            r#"{"refreshToken":"refresh"}"#
        ));
    }
}
