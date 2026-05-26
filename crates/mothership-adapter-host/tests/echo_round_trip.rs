//! End-to-end: spawn the sample echo adapter and drive the full protocol
//! (initialize -> identity -> models -> streamed chat).

use std::path::Path;

use mothership_adapter_host::protocol::ChatMessage;
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

    let models = adapter.models().expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "echo-1");
    assert!(models[0].recommended);

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
