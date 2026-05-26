//! End-to-end: spawn the sample echo adapter and drive the full protocol
//! (initialize -> identity -> models -> streamed chat).

use std::collections::BTreeMap;
use std::path::Path;

use mothership_adapter_host::protocol::{AuthKind, ChatMessage, ModelManagement, SettingsFieldKind};
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
    let full = adapter
        .chat(
            "echo-1",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hello brave world".to_string(),
            }],
            |delta| deltas.push(delta.to_string()),
        )
        .expect("chat");

    assert_eq!(full.trim(), "hello brave world");
    // Streamed word-by-word, not delivered in one shot.
    assert!(deltas.len() >= 3, "expected streamed deltas, got {deltas:?}");
}
