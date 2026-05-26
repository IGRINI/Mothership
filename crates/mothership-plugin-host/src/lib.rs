//! Host runtime for runtime-loadable provider-adapter plugins.
//!
//! Plugins are WebAssembly components. On load the host AOT-compiles each
//! component to native code (Cranelift), so calls execute native machine code
//! rather than an interpreter; the instance stays resident in memory and is
//! freed when [`LoadedPlugin`] is dropped (= unload). wasmtime lives in this
//! crate so cranelift never leaks into `mothership-core`.

use std::path::Path;

use anyhow::{Context, Result};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "provider-adapter",
});

/// Per-instance store state. A downloaded adapter sees only the host-provided
/// WASI capabilities wired into the linker — no ambient filesystem, network, or
/// environment — so its blast radius is its own linear memory plus whatever the
/// host explicitly grants.
struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Shared, reusable plugin runtime: the wasmtime engine plus a linker with the
/// host capabilities (currently WASI) pre-registered. One per app; load many
/// plugins from it.
pub struct PluginHost {
    engine: Engine,
    linker: Linker<HostState>,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        add_to_linker_sync(&mut linker).context("register WASI host capabilities")?;
        Ok(Self { engine, linker })
    }

    /// AOT-compiles the component at `path` to native code and instantiates it
    /// as a resident plugin. Dropping the returned [`LoadedPlugin`] unloads it.
    pub fn load(&self, path: &Path) -> Result<LoadedPlugin> {
        let wasm = std::fs::read(path)
            .with_context(|| format!("read plugin component {}", path.display()))?;

        // AOT: compile the portable component to native code for this machine.
        // In production this artifact is cached to disk (.cwasm) at install time
        // and mmap'd via `Component::deserialize_file`; here we keep it in memory.
        let precompiled = self
            .engine
            .precompile_component(&wasm)
            .context("AOT-compile plugin component")?;
        // SAFETY: `precompiled` was just produced by this same engine. A real
        // store path verifies a signature before trusting downloaded artifacts.
        let component = unsafe { Component::deserialize(&self.engine, &precompiled) }
            .context("load AOT-compiled component")?;

        let mut store = Store::new(
            &self.engine,
            HostState {
                table: ResourceTable::new(),
                wasi: WasiCtxBuilder::new().build(),
            },
        );
        let bindings = ProviderAdapter::instantiate(&mut store, &component, &self.linker)
            .context("instantiate plugin component")?;

        Ok(LoadedPlugin { store, bindings })
    }
}

/// A resident, instantiated plugin. Its native code lives in memory until this
/// value is dropped.
pub struct LoadedPlugin {
    store: Store<HostState>,
    bindings: ProviderAdapter,
}

impl LoadedPlugin {
    /// Reads the adapter's identity (host registers this and shows it in the UI).
    pub fn info(&mut self) -> Result<AdapterInfo> {
        self.bindings.call_info(&mut self.store)
    }

    /// Reads the adapter's bundled model catalog.
    pub fn bundled_models(&mut self) -> Result<Vec<ModelInfo>> {
        self.bindings.call_bundled_models(&mut self.store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_component_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/sample-adapter/target/wasm32-wasip2/release/sample_adapter.wasm")
    }

    #[test]
    fn loads_calls_and_unloads_sample_adapter() {
        let path = sample_component_path();
        if !path.exists() {
            eprintln!(
                "skipping: sample component not built at {} \
                 (build it with the managed gnu toolchain for wasm32-wasip2)",
                path.display()
            );
            return;
        }

        let host = PluginHost::new().expect("build plugin host");
        let mut plugin = host.load(&path).expect("load sample adapter");

        let info = plugin.info().expect("call info");
        assert_eq!(info.provider_id, "sample");
        assert_eq!(info.provider_label, "Sample Provider");
        assert_eq!(info.abi_version, 1);

        let models = plugin.bundled_models().expect("call bundled-models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "sample-1");
        assert!(models[0].recommended);

        // Drop = unload; native code and linear memory are freed here.
        drop(plugin);
    }
}
