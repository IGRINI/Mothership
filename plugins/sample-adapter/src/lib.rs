//! Minimal sample provider-adapter plugin, compiled to a WASM component.
//!
//! Exports the Phase 1 metadata contract (identity + bundled catalog) and a
//! `probe-status` diagnostic that drives a network request through the
//! host-provided `host-http` capability — the plugin itself never opens a
//! socket.

wit_bindgen::generate!({
    world: "provider-adapter",
    path: "../../crates/mothership-plugin-host/wit",
});

// Host capability imported from the world.
use mothership::plugin::host_http;

struct SampleAdapter;

impl Guest for SampleAdapter {
    fn info() -> AdapterInfo {
        AdapterInfo {
            provider_id: "sample".to_string(),
            provider_label: "Sample Provider".to_string(),
            abi_version: 1,
        }
    }

    fn bundled_models() -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "sample-1".to_string(),
            label: "Sample 1".to_string(),
            family: "Sample".to_string(),
            description: "A bundled model exported by the sample WASM adapter.".to_string(),
            recommended: true,
        }]
    }

    fn probe_status(url: String) -> Result<u16, String> {
        host_http::fetch_status(&url)
    }
}

export!(SampleAdapter);
