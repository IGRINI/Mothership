//! Minimal sample provider-adapter plugin, compiled to a WASM component.
//!
//! It exports only the Phase 1 metadata contract (identity + bundled catalog),
//! proving the host can load a component, read what it needs to show in the UI,
//! and unload it — with no host imports, so the component is fully self-contained.

wit_bindgen::generate!({
    world: "provider-adapter",
    path: "../../crates/mothership-plugin-host/wit",
});

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
}

export!(SampleAdapter);
