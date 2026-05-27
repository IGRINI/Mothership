//! A pool of resident adapter processes, keyed by provider.
//!
//! Spawning an adapter per operation is wasteful: each op pays a process start,
//! an `initialize`, and a `set_settings` push. The pool keeps **one resident
//! process per provider** and reuses it across model listing, settings reads,
//! and chat turns.
//!
//! Access is per-provider. If the resident process is busy (e.g. mid chat
//! stream) when another op arrives, that op falls back to a **one-off ephemeral
//! spawn** instead of blocking — so reuse is a fast-path optimization that never
//! regresses concurrency. On any protocol error the resident process is dropped,
//! so the next call respawns rather than reusing a desynced pipe.
//!
//! Settings are re-pushed to a resident adapter only when the vault changed
//! since the last push (the user saved new settings, or the adapter stored a
//! refreshed token), detected by hashing the stored settings map.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use mothership_adapter_host::{Adapter, AdapterEntry};

use crate::auth::FileCredentialVault;

/// Resident adapter processes shared across the sidecar's worker threads.
#[derive(Default)]
pub struct AdapterPool {
    slots: Mutex<HashMap<String, Arc<Mutex<Slot>>>>,
}

#[derive(Default)]
struct Slot {
    adapter: Option<Adapter>,
    settings_hash: u64,
}

impl AdapterPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `op` against a ready adapter for `entry`'s provider: the resident
    /// process if free (spawned + initialized + settings current on first use),
    /// or a one-off ephemeral spawn if the resident is busy.
    pub fn with<T>(
        &self,
        entry: &AdapterEntry,
        vault: &FileCredentialVault,
        op: impl FnOnce(&mut Adapter) -> Result<T>,
    ) -> Result<T> {
        let slot = self.slot(&entry.provider_id);
        // Bind to a local so the `try_lock` result temporary (its `Err` carries a
        // guard) drops before `slot` does, rather than living to end of block.
        let outcome = match slot.try_lock() {
            Ok(mut guard) => run_resident(&mut guard, entry, vault, op),
            // Resident busy → one-off spawn, untouched by the pool.
            Err(_) => {
                let (mut adapter, _) = spawn_ready(entry, vault)?;
                op(&mut adapter)
            }
        };
        outcome
    }

    /// Drops any resident adapter for a provider (e.g. after logout deleted its
    /// credential) so the next use starts from a clean process.
    pub fn evict(&self, provider_id: &str) {
        if let Some(slot) = self.slots.lock().unwrap().get(provider_id).cloned() {
            if let Ok(mut guard) = slot.try_lock() {
                guard.adapter = None;
                guard.settings_hash = 0;
            }
        }
    }

    fn slot(&self, provider_id: &str) -> Arc<Mutex<Slot>> {
        Arc::clone(
            self.slots
                .lock()
                .unwrap()
                .entry(provider_id.to_string())
                .or_default(),
        )
    }
}

fn run_resident<T>(
    slot: &mut Slot,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
    op: impl FnOnce(&mut Adapter) -> Result<T>,
) -> Result<T> {
    if slot.adapter.is_none() {
        let (adapter, hash) = spawn_ready(entry, vault)?;
        slot.adapter = Some(adapter);
        slot.settings_hash = hash;
    } else {
        // Re-push only if the stored settings changed since the last push.
        let settings = load_settings(entry, vault);
        let hash = settings_hash(&settings);
        if hash != slot.settings_hash {
            if let Some(adapter) = slot.adapter.as_mut() {
                adapter.set_settings(settings)?;
                slot.settings_hash = hash;
            }
        }
    }

    let adapter = slot.adapter.as_mut().expect("resident adapter present");
    match op(adapter) {
        Ok(value) => Ok(value),
        Err(error) => {
            // The exchange failed — the process may have died mid-protocol. Drop
            // it so the next call respawns instead of reusing a desynced pipe.
            slot.adapter = None;
            slot.settings_hash = 0;
            Err(error)
        }
    }
}

/// Spawns an adapter, wires the credential-store side channel, initializes it,
/// and pushes current settings. Returns the adapter and the hash of the pushed
/// settings (so the caller can detect later changes).
fn spawn_ready(entry: &AdapterEntry, vault: &FileCredentialVault) -> Result<(Adapter, u64)> {
    let mut adapter = Adapter::spawn(&entry.program)?;

    let sink_vault = vault.clone();
    let provider = entry.provider_id.clone();
    adapter.set_store_secret_handler(move |values| {
        if let Err(error) = sink_vault.merge_adapter_settings(&provider, values) {
            eprintln!("failed to persist adapter secret for {provider}: {error}");
        }
    });

    adapter.initialize()?;

    let settings = load_settings(entry, vault);
    let hash = settings_hash(&settings);
    if !settings.is_empty() {
        adapter.set_settings(settings)?;
    }
    Ok((adapter, hash))
}

fn load_settings(entry: &AdapterEntry, vault: &FileCredentialVault) -> BTreeMap<String, String> {
    vault
        .load_adapter_settings(&entry.provider_id)
        .unwrap_or_default()
}

fn settings_hash(settings: &BTreeMap<String, String>) -> u64 {
    let mut hasher = DefaultHasher::new();
    settings.hash(&mut hasher);
    hasher.finish()
}
