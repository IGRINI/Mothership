use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use mothership_core::Database;

pub struct AppState {
    database: Database,
    oauth_listener_active: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            oauth_listener_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn try_begin_oauth_listener(&self) -> bool {
        self.oauth_listener_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish_oauth_listener(&self) {
        self.oauth_listener_active.store(false, Ordering::SeqCst);
    }

    pub fn oauth_listener_active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.oauth_listener_active)
    }
}
