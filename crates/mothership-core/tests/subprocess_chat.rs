//! Proves the core can stream a chat through a subprocess adapter using its
//! uniform `LlmChatCompletionGateway` interface — driving the sample echo
//! adapter from `mothership-adapter-host`.

use std::path::PathBuf;

use mothership_core::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmTransportKind, SubprocessChatGateway,
};

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

    let gateway = SubprocessChatGateway::new(path);
    let mut sink = RecordingSink::default();

    let full = gateway
        .complete_chat(
            LlmChatCompletionRequest {
                provider_id: "echo".to_string(),
                model_id: "echo-1".to_string(),
                system_prompt: "be brief".to_string(),
                messages: vec![LlmChatMessage {
                    role: LlmChatRole::User,
                    content: "hello brave world".to_string(),
                }],
            },
            &mut sink,
        )
        .expect("chat through subprocess adapter");

    assert_eq!(full.trim(), "hello brave world");
    assert_eq!(sink.text.trim(), "hello brave world");
    assert_eq!(sink.transports, vec![LlmTransportKind::Subprocess]);
}
