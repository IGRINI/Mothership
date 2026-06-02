//! End-to-end: spawn the sample echo adapter and drive the full protocol
//! (initialize -> identity -> models -> streamed chat).

use std::collections::BTreeMap;
use std::path::Path;

use mothership_adapter_host::protocol::{
    AuthKind, ChatMessage, ModelManagement, PromptBundle, RuntimeContext, SettingsFieldKind,
};
use mothership_adapter_host::Adapter;

#[test]
fn echo_adapter_round_trip() {
    // Cargo sets CARGO_BIN_EXE_<bin> for integration tests, pointing at the
    // freshly built sample adapter binary.
    let exe = env!("CARGO_BIN_EXE_echo_adapter");
    let mut adapter = Adapter::spawn(Path::new(exe)).expect("spawn echo adapter");

    adapter.initialize().expect("initialize");

    let (provider_id, label) = adapter.identity().expect("identity");
    assert_eq!(provider_id, "echo");
    assert_eq!(label, "Echo Provider");

    let (models, management) = adapter.models().expect("models");
    assert_eq!(management, ModelManagement::Fixed);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "echo-1");
    assert!(models[0].recommended);

    let fields = adapter.settings_schema().expect("settings schema");
    assert!(fields
        .iter()
        .any(|field| field.key == "api_key" && matches!(field.kind, SettingsFieldKind::Secret)));

    adapter
        .set_settings(BTreeMap::from([(
            "endpoint".to_string(),
            "https://example".to_string(),
        )]))
        .expect("set settings");

    let auth = adapter.auth_schema().expect("auth schema");
    assert!(matches!(auth, AuthKind::ApiKey { .. }));

    let mut deltas = Vec::new();
    let round = adapter
        .chat_round_cancellable(
            "echo-1",
            None,
            PromptBundle::default(),
            RuntimeContext::default(),
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hello brave world".to_string(),
            }],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            || false,
            |delta| deltas.push(delta.to_string()),
        )
        .expect("chat round");

    assert_eq!(round.text.trim(), "hello brave world");
    assert!(round.tool_calls.is_empty());
    // Streamed word-by-word, not delivered in one shot.
    assert!(
        deltas.len() >= 3,
        "expected streamed deltas, got {deltas:?}"
    );
}

/// The adapter->host `StoreSecret` side channel: the adapter can push a secret
/// at any point and the host consumes it transparently inside `recv`, invoking
/// the registered handler, without disturbing the in-flight request/response.
#[test]
fn store_secret_is_forwarded_to_handler() {
    use std::sync::{Arc, Mutex};

    let exe = env!("CARGO_BIN_EXE_echo_adapter");
    let mut adapter = Adapter::spawn(Path::new(exe)).expect("spawn echo adapter");

    let captured = Arc::new(Mutex::new(Vec::<BTreeMap<String, String>>::new()));
    let sink = Arc::clone(&captured);
    adapter.set_store_secret_handler(move |values| {
        sink.lock().expect("lock").push(values);
    });

    adapter.initialize().expect("initialize");

    // The echo adapter emits a `StoreSecret` *before* acking when this key is
    // present, so the ack still resolves `set_settings` while the secret is
    // routed to the handler.
    adapter
        .set_settings(BTreeMap::from([(
            "echo_store_secret".to_string(),
            "token-xyz".to_string(),
        )]))
        .expect("set settings");

    let captured = captured.lock().expect("lock");
    assert_eq!(
        captured.len(),
        1,
        "expected exactly one StoreSecret, got {captured:?}"
    );
    assert_eq!(captured[0].get("persisted"), Some(&"token-xyz".to_string()));
}
