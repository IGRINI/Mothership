//! Proves the core can stream a model round through a subprocess adapter using
//! its uniform `LlmChatRoundGateway` interface — driving the sample echo
//! adapter from `mothership-adapter-host`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mothership_adapter_host::protocol::{PromptBundle, RuntimeContext};
use mothership_adapter_host::AdapterEntry;
use mothership_core::auth::FileCredentialVault;
use mothership_core::{
    AdapterPool, ChatCancellationToken, LlmChatCompletionEventSink, LlmChatCompletionRequest,
    LlmChatMessage, LlmChatRole, LlmChatRoundGateway, LlmChatRoundRequest, LlmTransportKind,
    SubprocessChatGateway,
};

/// Builds a gateway over a fresh single-process pool pointed at the echo adapter.
fn echo_gateway(path: PathBuf, vault: FileCredentialVault) -> SubprocessChatGateway {
    let entry = AdapterEntry {
        provider_id: "echo".to_string(),
        provider_label: "Echo".to_string(),
        program: path,
        icon: None,
        integrity: None,
        capabilities: BTreeSet::from(["llm.chat".to_string()]),
    };
    SubprocessChatGateway::new(Arc::new(AdapterPool::new()), entry, vault)
}

/// A `FileCredentialVault` rooted at a unique temp directory, so each test gets
/// an isolated shared-credential store.
fn temp_vault(name: &str) -> FileCredentialVault {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mothership_vault_{name}_{stamp}"));
    FileCredentialVault::new(root)
}

#[derive(Default)]
struct RecordingSink {
    text: String,
    transports: Vec<LlmTransportKind>,
}

impl LlmChatCompletionEventSink for RecordingSink {
    fn transport_selected(&mut self, transport: LlmTransportKind) {
        self.transports.push(transport);
    }

    fn delta(&mut self, delta: &str) {
        self.text.push_str(delta);
    }
}

/// The echo adapter binary built by `mothership-adapter-host`. Built into the
/// shared workspace target dir; the test skips if the workspace wasn't built.
fn echo_adapter_path() -> PathBuf {
    let exe = if cfg!(windows) {
        "echo_adapter.exe"
    } else {
        "echo_adapter"
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(exe)
}

#[test]
fn streams_chat_through_subprocess_adapter() {
    let path = echo_adapter_path();
    if !path.exists() {
        eprintln!(
            "skipping: echo adapter not built at {} (run `cargo build` first)",
            path.display()
        );
        return;
    }

    let gateway = echo_gateway(path, temp_vault("stream"));
    let mut sink = RecordingSink::default();

    let round = gateway
        .complete_round(
            LlmChatRoundRequest::from_completion(LlmChatCompletionRequest {
                provider_id: "echo".to_string(),
                model_id: "echo-1".to_string(),
                reasoning: None,
                fast_mode: false,
                prompt: PromptBundle::default(),
                runtime_context: RuntimeContext::default(),
                tools: Vec::new(),
                messages: vec![LlmChatMessage {
                    role: LlmChatRole::User,
                    content: "hello brave world".to_string(),
                }],
            }),
            &ChatCancellationToken::default(),
            &mut sink,
        )
        .expect("round through subprocess adapter");

    assert_eq!(round.text.trim(), "hello brave world");
    assert!(round.tool_calls.is_empty());
    assert_eq!(sink.text.trim(), "hello brave world");
    assert_eq!(sink.transports, vec![LlmTransportKind::Subprocess]);
}

#[test]
fn pushes_settings_before_streaming() {
    let path = echo_adapter_path();
    if !path.exists() {
        eprintln!("skipping: echo adapter not built at {}", path.display());
        return;
    }

    let settings = std::collections::BTreeMap::from([
        ("api_key".to_string(), "secret-value".to_string()),
        ("endpoint".to_string(), "https://example".to_string()),
    ]);
    // Seed the shared vault; the gateway loads these and pushes them to the
    // adapter via set_settings on spawn — secrets never live next to the adapter.
    let vault = temp_vault("settings");
    vault
        .save_adapter_settings("echo", &settings)
        .expect("seed adapter settings");
    let gateway = echo_gateway(path, vault);
    let mut sink = RecordingSink::default();

    let round = gateway
        .complete_round(
            LlmChatRoundRequest::from_completion(LlmChatCompletionRequest {
                provider_id: "echo".to_string(),
                model_id: "echo-1".to_string(),
                reasoning: None,
                fast_mode: false,
                prompt: PromptBundle::default(),
                runtime_context: RuntimeContext::default(),
                tools: Vec::new(),
                messages: vec![LlmChatMessage {
                    role: LlmChatRole::User,
                    content: "ping pong".to_string(),
                }],
            }),
            &ChatCancellationToken::default(),
            &mut sink,
        )
        .expect("round after settings push");

    // The adapter accepted set_settings (no error) and still streamed.
    assert_eq!(round.text.trim(), "ping pong");
}
