//! Host runtime for runtime-loadable provider-adapter plugins.
//!
//! Plugins are WebAssembly components. On load the host AOT-compiles each
//! component to native code (Cranelift), so calls execute native machine code
//! rather than an interpreter; the instance stays resident in memory and is
//! freed when [`LoadedPlugin`] is dropped (= unload). wasmtime lives in this
//! crate so cranelift never leaks into `mothership-core`.
//!
//! Network access follows decision B: the plugin never opens a socket. It calls
//! the host-provided `host-http` capability and the host owns the request —
//! socket, TLS, timeouts, observability and (later) auth injection. The backend
//! is injectable so tests exercise the boundary without touching the network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "provider-adapter",
});

use mothership::plugin::host_http;

/// Host-owned network backend. The plugin calls `host-http.fetch-status`; the
/// host performs the request here. Injectable so tests don't hit the network.
pub trait HttpBackend: Send + Sync {
    fn fetch_status(&self, url: &str) -> std::result::Result<u16, String>;
}

/// Default backend: a real blocking HTTP client.
pub struct ReqwestBackend {
    client: reqwest::blocking::Client,
}

impl ReqwestBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .build()
                .context("build HTTP client")?,
        })
    }
}

impl HttpBackend for ReqwestBackend {
    fn fetch_status(&self, url: &str) -> std::result::Result<u16, String> {
        self.client
            .get(url)
            .send()
            .map(|response| response.status().as_u16())
            .map_err(|error| error.to_string())
    }
}

/// Per-instance store state. A downloaded adapter sees only the host-provided
/// capabilities wired into the linker (WASI + `host-http`) — no ambient
/// filesystem, network, or environment — so its blast radius is its own linear
/// memory plus whatever the host explicitly grants.
struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: Arc<dyn HttpBackend>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl host_http::Host for HostState {
    fn fetch_status(&mut self, url: String) -> std::result::Result<u16, String> {
        self.http.fetch_status(&url)
    }
}

/// Shared, reusable plugin runtime: the wasmtime engine plus a linker with the
/// host capabilities (WASI + `host-http`) pre-registered. One per app; load many
/// plugins from it.
pub struct PluginHost {
    engine: Engine,
    linker: Linker<HostState>,
    http: Arc<dyn HttpBackend>,
}

impl PluginHost {
    /// Builds a host with the default (real) HTTP backend.
    pub fn new() -> Result<Self> {
        Self::with_http_backend(Arc::new(ReqwestBackend::new()?))
    }

    /// Builds a host with a caller-provided HTTP backend (tests inject a fake).
    pub fn with_http_backend(http: Arc<dyn HttpBackend>) -> Result<Self> {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        add_to_linker_sync(&mut linker).context("register WASI host capabilities")?;
        host_http::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |state| state)
            .context("register host-http capability")?;
        Ok(Self {
            engine,
            linker,
            http,
        })
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
                http: self.http.clone(),
            },
        );
        let bindings = ProviderAdapter::instantiate(&mut store, &component, &self.linker)
            .context("instantiate plugin component")?;

        Ok(LoadedPlugin { store, bindings })
    }

    /// Loads a component, reads its manifest once, and unloads it. Use when only
    /// the metadata is needed (e.g. to register the provider in the UI).
    pub fn load_manifest(&self, path: &Path) -> Result<AdapterManifest> {
        self.load(path)?.manifest()
    }

    /// Scans `dir` for `*.wasm` plugins and returns each path with its manifest.
    /// A plugin that fails to load/validate is reported in place and skipped, so
    /// one bad plugin can't break the rest of the list. A missing dir is empty.
    pub fn scan(&self, dir: &Path) -> Vec<(PathBuf, Result<AdapterManifest>)> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("wasm") {
                let manifest = self.load_manifest(&path);
                found.push((path, manifest));
            }
        }
        found
    }
}

/// Stable, owned snapshot of a plugin's identity + catalog, read once at load.
/// Lets the app register a plugin (show it in the model picker) without touching
/// wasm bindgen types or keeping the instance resident.
#[derive(Debug, Clone)]
pub struct AdapterManifest {
    pub provider_id: String,
    pub provider_label: String,
    pub abi_version: u32,
    pub models: Vec<AdapterModel>,
}

/// One model a plugin advertises in its bundled catalog.
#[derive(Debug, Clone)]
pub struct AdapterModel {
    pub id: String,
    pub label: String,
    pub family: String,
    pub description: String,
    pub recommended: bool,
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

    /// Drives the host-http capability from inside the plugin and returns the
    /// status it observed (diagnostic; proves the host-owned-network boundary).
    pub fn probe_status(&mut self, url: &str) -> Result<u16> {
        self.bindings
            .call_probe_status(&mut self.store, url)?
            .map_err(|error| anyhow::anyhow!("plugin probe-status failed: {error}"))
    }

    /// Reads the plugin's identity + bundled catalog into an owned manifest.
    pub fn manifest(&mut self) -> Result<AdapterManifest> {
        let info = self.info()?;
        let models = self
            .bundled_models()?
            .into_iter()
            .map(|model| AdapterModel {
                id: model.id,
                label: model.label,
                family: model.family,
                description: model.description,
                recommended: model.recommended,
            })
            .collect();
        Ok(AdapterManifest {
            provider_id: info.provider_id,
            provider_label: info.provider_label,
            abi_version: info.abi_version,
            models,
        })
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

    /// Deterministic backend so the host-http test never touches the network.
    struct FakeHttp;
    impl HttpBackend for FakeHttp {
        fn fetch_status(&self, _url: &str) -> std::result::Result<u16, String> {
            Ok(218)
        }
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

    #[test]
    fn plugin_drives_host_http_capability() {
        let path = sample_component_path();
        if !path.exists() {
            eprintln!("skipping: sample component not built at {}", path.display());
            return;
        }

        let host = PluginHost::with_http_backend(Arc::new(FakeHttp)).expect("build plugin host");
        let mut plugin = host.load(&path).expect("load sample adapter");

        // 218 is a sentinel from the fake backend: the value crossed
        // guest -> host-http -> guest without the plugin owning a socket.
        let status = plugin
            .probe_status("https://example.invalid/x")
            .expect("probe-status");
        assert_eq!(status, 218);
    }

    #[test]
    fn reads_manifest_from_component() {
        let path = sample_component_path();
        if !path.exists() {
            eprintln!("skipping: sample component not built at {}", path.display());
            return;
        }
        let host = PluginHost::with_http_backend(Arc::new(FakeHttp)).expect("build plugin host");
        let manifest = host.load_manifest(&path).expect("read manifest");
        assert_eq!(manifest.provider_id, "sample");
        assert_eq!(manifest.provider_label, "Sample Provider");
        assert_eq!(manifest.models.len(), 1);
        assert!(manifest.models[0].recommended);
    }

    #[test]
    fn scan_finds_sample_plugin() {
        let path = sample_component_path();
        if !path.exists() {
            eprintln!("skipping: sample component not built");
            return;
        }
        let dir = path.parent().expect("plugin dir");
        let host = PluginHost::with_http_backend(Arc::new(FakeHttp)).expect("build plugin host");
        let sample = host
            .scan(dir)
            .into_iter()
            .filter_map(|(_, manifest)| manifest.ok())
            .find(|manifest| manifest.provider_id == "sample");
        assert!(sample.is_some(), "scan should find the sample plugin manifest");
    }
}
