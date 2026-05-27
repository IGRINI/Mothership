use crate::sidecar::Sidecar;

/// Host application state. The host is a thin client: it owns no database,
/// adapters, or credential vault — only a handle to the Core sidecar it talks to.
pub struct AppState {
    sidecar: Sidecar,
}

impl AppState {
    pub fn new(sidecar: Sidecar) -> Self {
        Self { sidecar }
    }

    pub fn sidecar(&self) -> &Sidecar {
        &self.sidecar
    }
}
