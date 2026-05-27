use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use mothership_core::Database;

pub struct AppState {
    database: Database,
    /// In-flight `authenticate` flows, by provider id -> adapter process id, so
    /// the UI can cancel one (or leaving Settings can) by terminating it.
    auth_processes: Arc<Mutex<HashMap<String, u32>>>,
}

impl AppState {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            auth_processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Removes and returns the in-flight `authenticate` pid for `provider_id`.
    /// Returns `None` if there isn't one (or another caller already took it —
    /// which is how the authenticate command learns it was cancelled).
    pub fn take_auth_process(&self, provider_id: &str) -> Option<u32> {
        self.auth_processes
            .lock()
            .ok()
            .and_then(|mut map| map.remove(provider_id))
    }

    /// A shared handle to the in-flight auth-process table, so the blocking
    /// `authenticate` work (run off-thread) can register/clear its pid without
    /// borrowing the non-`Send` `State`.
    pub fn auth_processes(&self) -> Arc<Mutex<HashMap<String, u32>>> {
        Arc::clone(&self.auth_processes)
    }
}
