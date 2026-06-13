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
//!
//! Because an operation may end up on either the resident or an ephemeral
//! process, callers that need a process-level kill switch (the chat cancel
//! fallback) must not assume "the provider's resident" is their process. The
//! pool therefore reports the **actual** process serving an operation through a
//! scoped, thread-local observer ([`with_adapter_use_observer`]) as an
//! [`AdapterKillHandle`] that kills exactly that process — and only while it is
//! still the process the handle was minted for.

use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use mothership_adapter_host::{Adapter, AdapterEntry};

use crate::auth::FileCredentialVault;

pub struct PreparedAdapter {
    pub adapter: Adapter,
    pub settings_hash: u64,
}

/// Resident adapter processes shared across the sidecar's worker threads.
#[derive(Default)]
pub struct AdapterPool {
    slots: Mutex<HashMap<String, Arc<ProviderSlot>>>,
}

#[derive(Default)]
struct Slot {
    adapter: Option<Adapter>,
    settings_hash: u64,
}

#[derive(Default)]
struct ProviderSlot {
    inner: Mutex<Slot>,
    /// Identity of the current resident process: `(generation << 32) | pid`,
    /// 0 when none. The generation makes every resident incarnation unique, so
    /// a stale kill handle can never hit a later resident that happened to
    /// recycle the same pid.
    resident_token: AtomicU64,
    /// Monotonic generation source for `resident_token`; never reset.
    next_generation: AtomicU32,
}

fn resident_token(generation: u32, pid: u32) -> u64 {
    (u64::from(generation) << 32) | u64::from(pid)
}

fn resident_pid_of(token: u64) -> u32 {
    token as u32
}

impl ProviderSlot {
    /// Kills the resident process (its whole tree) and clears the slot's
    /// bookkeeping. With `only_token`, acts only when that exact resident
    /// incarnation is still current — a stale handle must not touch a
    /// replacement resident, even one that recycled the same pid.
    fn evict(&self, only_token: Option<u64>, kill: impl FnOnce(u32)) {
        let token = match only_token {
            None => self.resident_token.swap(0, Ordering::SeqCst),
            Some(expected) => {
                if expected == 0
                    || self
                        .resident_token
                        .compare_exchange(expected, 0, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    return;
                }
                expected
            }
        };
        let pid = resident_pid_of(token);
        if pid != 0 {
            kill(pid);
        }
        if let Ok(mut guard) = self.inner.try_lock() {
            guard.adapter = None;
            guard.settings_hash = 0;
        }
    }
}

/// Identifies the adapter process that served (or is serving) one pool
/// operation, and can kill exactly that process.
///
/// - For a **resident** process, killing goes through the pool's eviction
///   bookkeeping, guarded by the generation+pid token recorded at mint time:
///   if the resident was replaced since — even by one recycling the same pid —
///   the handle does nothing instead of killing another run's healthy process.
/// - For an **ephemeral** process, killing is guarded by an alive flag the
///   pool clears the moment the operation finishes (before the process is
///   reaped), so a late kill can never hit a recycled pid.
#[derive(Clone)]
pub struct AdapterKillHandle {
    pid: u32,
    target: KillTarget,
}

#[derive(Clone)]
enum KillTarget {
    Resident { slot: Arc<ProviderSlot>, token: u64 },
    Ephemeral { alive: Arc<AtomicBool> },
}

impl AdapterKillHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Kills the tracked adapter process tree, if it is still the process this
    /// handle was minted for. Safe to call at any time; stale handles no-op.
    pub fn kill(&self) {
        self.kill_with(kill_process_tree);
    }

    fn kill_with(&self, kill: impl FnOnce(u32)) {
        match &self.target {
            KillTarget::Resident { slot, token } => slot.evict(Some(*token), kill),
            KillTarget::Ephemeral { alive } => {
                if alive.swap(false, Ordering::SeqCst) {
                    kill(self.pid);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_for_tests(pid: u32) -> Self {
        Self {
            pid,
            target: KillTarget::Ephemeral {
                alive: Arc::new(AtomicBool::new(true)),
            },
        }
    }
}

type AdapterUseObserver = Box<dyn FnOnce(AdapterKillHandle)>;

thread_local! {
    static ADAPTER_USE_OBSERVER: Cell<Option<AdapterUseObserver>> = const { Cell::new(None) };
}

/// Runs `f` with a one-shot observer installed on this thread: the **first**
/// adapter the pool serves inside `f` — resident or ephemeral — is reported as
/// an [`AdapterKillHandle`]. Later pool uses on the same thread (e.g. a
/// mid-turn tool that itself talks through an adapter) are not reported; the
/// first acquisition is the one serving `f`'s operation.
///
/// This is a thread-local side channel because the pool call sits below
/// layers that don't know which run they serve, while the pool's caller knows
/// the run but not the process. mothership-core is sync (thread-per-request),
/// so the scope is well-defined.
pub(crate) fn with_adapter_use_observer<R>(
    observer: impl FnOnce(AdapterKillHandle) + 'static,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<AdapterUseObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ADAPTER_USE_OBSERVER.with(|cell| cell.set(self.0.take()));
        }
    }

    let previous = ADAPTER_USE_OBSERVER.with(|cell| cell.replace(Some(Box::new(observer))));
    let _restore = Restore(previous);
    f()
}

/// Reports the adapter serving the current pool operation to the observer
/// installed on this thread, if any. `handle` is built lazily so the common
/// unobserved path (model listing, auth flows) pays nothing.
fn notify_adapter_use(handle: impl FnOnce() -> AdapterKillHandle) {
    if let Some(observer) = ADAPTER_USE_OBSERVER.with(|cell| cell.take()) {
        observer(handle());
    }
}

/// Clears an ephemeral process's alive flag when its operation ends — on
/// success, error, or panic — strictly before the process is reaped, so an
/// [`AdapterKillHandle`] outliving the operation can never kill a recycled pid.
struct ClearAliveOnDrop(Arc<AtomicBool>);

impl Drop for ClearAliveOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
        self.with_spawner(entry, vault, spawn_ready_adapter, op)
    }

    /// [`Self::with`] with the process spawn injectable — the seam unit tests
    /// use to drive pool bookkeeping without real adapter binaries.
    fn with_spawner<T>(
        &self,
        entry: &AdapterEntry,
        vault: &FileCredentialVault,
        spawner: impl FnOnce(&AdapterEntry, &FileCredentialVault) -> Result<PreparedAdapter>,
        op: impl FnOnce(&mut Adapter) -> Result<T>,
    ) -> Result<T> {
        let slot = self.slot(&entry.provider_id);
        // Bind to a local so the `try_lock` result temporary (its `Err` carries a
        // guard) drops before `slot` does, rather than living to end of block.
        let outcome = match slot.inner.try_lock() {
            Ok(mut guard) => run_resident(&slot, &mut guard, entry, vault, spawner, op),
            // Resident busy → one-off spawn, untouched by the pool.
            Err(_) => {
                let mut session = spawner(entry, vault)?;
                let alive = Arc::new(AtomicBool::new(true));
                // Declared after `session`, so it drops first: the flag goes
                // false before the process is killed and reaped on drop.
                let _alive_guard = ClearAliveOnDrop(Arc::clone(&alive));
                let pid = session.adapter.process_id();
                notify_adapter_use(|| AdapterKillHandle {
                    pid,
                    target: KillTarget::Ephemeral {
                        alive: Arc::clone(&alive),
                    },
                });
                op(&mut session.adapter)
            }
        };
        outcome
    }

    /// Drops any resident adapter for a provider (e.g. after logout deleted its
    /// credential) so the next use starts from a clean process.
    pub fn evict(&self, provider_id: &str) {
        self.force_evict(provider_id);
    }

    /// Terminates and drops a provider's resident adapter even when it is busy.
    /// This is the enforcement path for logout / secret clear: after it returns,
    /// the old resident process cannot continue using stale credentials. A busy
    /// operation will observe a broken pipe/EOF and fail its run.
    pub fn force_evict(&self, provider_id: &str) {
        if let Some(slot) = self.slots.lock().unwrap().get(provider_id).cloned() {
            slot.evict(None, kill_process_tree);
        }
    }

    pub fn is_resident_ready(&self, provider_id: &str) -> bool {
        self.slots
            .lock()
            .unwrap()
            .get(provider_id)
            .map(|slot| slot.resident_token.load(Ordering::SeqCst) != 0)
            .unwrap_or(false)
    }

    fn slot(&self, provider_id: &str) -> Arc<ProviderSlot> {
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
    provider_slot: &Arc<ProviderSlot>,
    slot: &mut Slot,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
    spawner: impl FnOnce(&AdapterEntry, &FileCredentialVault) -> Result<PreparedAdapter>,
    op: impl FnOnce(&mut Adapter) -> Result<T>,
) -> Result<T> {
    let freshly_spawned = slot.adapter.is_none();
    if freshly_spawned {
        let session = spawner(entry, vault)?;
        let generation = provider_slot.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        provider_slot.resident_token.store(
            resident_token(generation, session.adapter.process_id()),
            Ordering::SeqCst,
        );
        slot.settings_hash = session.settings_hash;
        slot.adapter = Some(session.adapter);
    }

    let adapter = slot.adapter.as_mut().expect("resident adapter present");
    let pid = adapter.process_id();
    let token = provider_slot.resident_token.load(Ordering::SeqCst);
    notify_adapter_use(|| AdapterKillHandle {
        pid,
        target: KillTarget::Resident {
            slot: Arc::clone(provider_slot),
            token,
        },
    });

    if !freshly_spawned {
        // Re-push only if the stored settings changed since the last push.
        let settings = load_settings(entry, vault);
        let hash = settings_hash(&settings);
        if hash != slot.settings_hash {
            adapter.set_settings(settings)?;
            slot.settings_hash = hash;
        }
    }

    match op(adapter) {
        Ok(value) => Ok(value),
        Err(error) => {
            // The exchange failed — the process may have died mid-protocol. Drop
            // it so the next call respawns instead of reusing a desynced pipe.
            slot.adapter = None;
            slot.settings_hash = 0;
            provider_slot.resident_token.store(0, Ordering::SeqCst);
            Err(error)
        }
    }
}

/// Spawns an adapter, wires the credential-store side channel, initializes it,
/// and pushes current settings. Returns the adapter and the hash of the pushed
/// settings (so the caller can detect later changes).
pub fn spawn_ready_adapter(
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> Result<PreparedAdapter> {
    let settings = load_settings(entry, vault);
    spawn_ready_adapter_with_settings(entry, vault, settings)
}

/// Like [`spawn_ready_adapter`], but uses an explicit settings snapshot while
/// still wiring `StoreSecret` into the supplied vault.
pub fn spawn_ready_adapter_with_settings(
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
    settings: BTreeMap<String, String>,
) -> Result<PreparedAdapter> {
    spawn_ready_adapter_session(entry, settings, Some(vault))
}

/// Like [`spawn_ready_adapter_with_settings`], but intentionally does not wire
/// `StoreSecret`. Logout/revoke flows must be able to seed the adapter with the
/// old token without letting that adapter re-persist it after Core has deleted
/// the vault entry.
pub fn spawn_ready_adapter_without_secret_sink(
    entry: &AdapterEntry,
    settings: BTreeMap<String, String>,
) -> Result<PreparedAdapter> {
    spawn_ready_adapter_session(entry, settings, None)
}

fn spawn_ready_adapter_session(
    entry: &AdapterEntry,
    settings: BTreeMap<String, String>,
    secret_vault: Option<&FileCredentialVault>,
) -> Result<PreparedAdapter> {
    let mut adapter = Adapter::spawn(&entry.program)?;

    if let Some(vault) = secret_vault {
        let sink_vault = vault.clone();
        let provider = entry.provider_id.clone();
        adapter.set_store_secret_handler(move |values| {
            if let Err(error) = sink_vault.merge_adapter_settings(&provider, values) {
                eprintln!("failed to persist adapter secret for {provider}: {error}");
            }
        });
    }

    adapter.initialize()?;

    let hash = settings_hash(&settings);
    if !settings.is_empty() {
        adapter.set_settings(settings)?;
    }
    Ok(PreparedAdapter {
        adapter,
        settings_hash: hash,
    })
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

/// Best-effort kill of a process **and its children** (`taskkill /T` on
/// Windows). The single kill helper for adapter processes — pool eviction and
/// the auth-cancel path both go through it, so tree semantics stay consistent.
/// Blocks until the kill command finishes (a few ms), which lets callers like
/// [`AdapterPool::force_evict`] guarantee the process is gone on return.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_entry(provider_id: &str) -> AdapterEntry {
        AdapterEntry {
            provider_id: provider_id.to_string(),
            provider_label: provider_id.to_string(),
            // Never spawned: tests inject their own spawner.
            program: PathBuf::from("unused-test-program"),
            icon: None,
            integrity: None,
            capabilities: BTreeSet::new(),
        }
    }

    fn test_vault(name: &str) -> FileCredentialVault {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        // The directory is never created: an empty vault yields empty settings,
        // which hash to `empty_settings_hash`, so the resident-reuse path skips
        // the `set_settings` re-push (no protocol traffic to the fake process).
        FileCredentialVault::new(std::env::temp_dir().join(format!("ms_pool_{name}_{stamp}")))
    }

    fn empty_settings_hash() -> u64 {
        settings_hash(&BTreeMap::new())
    }

    /// A real but inert child process standing in for an adapter: `sort` (both
    /// System32 and POSIX) blocks reading the stdin pipe we hold open, and is
    /// killed + reaped when the `Adapter` drops. No adapter protocol is ever
    /// exchanged with it.
    fn spawn_inert_process() -> Adapter {
        Adapter::spawn(Path::new("sort")).expect("spawn inert test process")
    }

    fn counting_spawner(
        spawns: &Arc<AtomicUsize>,
    ) -> impl FnOnce(&AdapterEntry, &FileCredentialVault) -> Result<PreparedAdapter> {
        let spawns = Arc::clone(spawns);
        move |_, _| {
            spawns.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedAdapter {
                adapter: spawn_inert_process(),
                settings_hash: empty_settings_hash(),
            })
        }
    }

    fn observed_handle() -> (
        impl FnOnce(AdapterKillHandle) + 'static,
        Arc<Mutex<Vec<AdapterKillHandle>>>,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (move |handle| sink.lock().unwrap().push(handle), seen)
    }

    #[test]
    fn resident_is_reused_when_free() {
        let pool = AdapterPool::new();
        let entry = test_entry("reuse");
        let vault = test_vault("reuse");
        let spawns = Arc::new(AtomicUsize::new(0));

        let first = pool
            .with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
            .expect("first round");
        let second = pool
            .with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
            .expect("second round");

        assert_eq!(first, second, "free resident must be reused");
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "no respawn for reuse");
        assert!(pool.is_resident_ready("reuse"));
    }

    #[test]
    fn busy_resident_falls_back_to_ephemeral_and_kill_targets_it() {
        let pool = Arc::new(AdapterPool::new());
        let entry = test_entry("busy");
        let vault = test_vault("busy");
        let spawns = Arc::new(AtomicUsize::new(0));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let resident_thread = {
            let pool = Arc::clone(&pool);
            let entry = entry.clone();
            let vault = vault.clone();
            let spawns = Arc::clone(&spawns);
            thread::spawn(move || {
                pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                    entered_tx.send(adapter.process_id()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(adapter.process_id())
                })
            })
        };
        let resident_pid = entered_rx.recv().expect("resident round entered");

        // While the resident is busy, a concurrent round must go ephemeral, and
        // the kill handle reported for it must target the ephemeral process.
        let (observer, seen) = observed_handle();
        let seen_in_op = Arc::clone(&seen);
        let ephemeral_pid = with_adapter_use_observer(observer, || {
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                let handle = seen_in_op.lock().unwrap().first().cloned().expect("handle");
                let mut killed = Vec::new();
                handle.kill_with(|pid| killed.push(pid));
                assert_eq!(
                    killed,
                    vec![adapter.process_id()],
                    "ephemeral handle must kill the ephemeral process"
                );
                Ok(adapter.process_id())
            })
        })
        .expect("ephemeral round");

        assert_ne!(ephemeral_pid, resident_pid);
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
        let handle = seen.lock().unwrap().first().cloned().expect("handle");
        assert_eq!(handle.pid(), ephemeral_pid);
        // The resident's bookkeeping must be untouched by the ephemeral kill.
        assert!(pool.is_resident_ready("busy"));

        release_tx.send(()).unwrap();
        let resident_round_pid = resident_thread
            .join()
            .expect("resident thread")
            .expect("resident round");
        assert_eq!(resident_round_pid, resident_pid);
    }

    #[test]
    fn resident_round_kill_handle_evicts_resident() {
        let pool = AdapterPool::new();
        let entry = test_entry("evict");
        let vault = test_vault("evict");
        let spawns = Arc::new(AtomicUsize::new(0));

        let (observer, seen) = observed_handle();
        let resident_pid = with_adapter_use_observer(observer, || {
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
        })
        .expect("resident round");

        let handle = seen.lock().unwrap().first().cloned().expect("handle");
        assert_eq!(
            handle.pid(),
            resident_pid,
            "free pool must serve (and report) the resident"
        );

        let mut killed = Vec::new();
        handle.kill_with(|pid| killed.push(pid));
        assert_eq!(killed, vec![resident_pid]);
        assert!(
            !pool.is_resident_ready("evict"),
            "killing through a resident handle must clear the slot"
        );

        // The next round respawns rather than reusing the killed process.
        pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
            Ok(adapter.process_id())
        })
        .expect("respawned round");
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
        assert!(pool.is_resident_ready("evict"));
    }

    #[test]
    fn stale_resident_handle_skips_respawned_resident() {
        let pool = AdapterPool::new();
        let entry = test_entry("stale");
        let vault = test_vault("stale");
        let spawns = Arc::new(AtomicUsize::new(0));

        let (observer, seen) = observed_handle();
        let old_pid = with_adapter_use_observer(observer, || {
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
        })
        .expect("first round");
        let stale_handle = seen.lock().unwrap().first().cloned().expect("handle");
        assert_eq!(stale_handle.pid(), old_pid);

        // A failed round drops the resident; the next round respawns.
        let failed: Result<()> =
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |_| {
                Err(anyhow::anyhow!("boom"))
            });
        assert!(failed.is_err());
        pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
            Ok(adapter.process_id())
        })
        .expect("respawned round");
        assert_eq!(spawns.load(Ordering::SeqCst), 2);

        // The stale handle must not kill the replacement resident — its
        // generation token no longer matches, even if the OS recycled the pid.
        let mut killed = Vec::new();
        stale_handle.kill_with(|pid| killed.push(pid));
        assert!(killed.is_empty(), "stale handle must be inert");
        assert!(pool.is_resident_ready("stale"));
    }

    #[test]
    fn ephemeral_kill_handle_is_inert_after_round() {
        let pool = Arc::new(AdapterPool::new());
        let entry = test_entry("inert");
        let vault = test_vault("inert");
        let spawns = Arc::new(AtomicUsize::new(0));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let resident_thread = {
            let pool = Arc::clone(&pool);
            let entry = entry.clone();
            let vault = vault.clone();
            let spawns = Arc::clone(&spawns);
            thread::spawn(move || {
                pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(adapter.process_id())
                })
            })
        };
        entered_rx.recv().expect("resident round entered");

        let (observer, seen) = observed_handle();
        with_adapter_use_observer(observer, || {
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
        })
        .expect("ephemeral round");

        // The round is over and the ephemeral process was reaped: a late kill
        // through the handle must not touch the (possibly recycled) pid.
        let handle = seen.lock().unwrap().first().cloned().expect("handle");
        let mut killed = Vec::new();
        handle.kill_with(|pid| killed.push(pid));
        assert!(killed.is_empty(), "completed ephemeral must not be killed");

        release_tx.send(()).unwrap();
        resident_thread
            .join()
            .expect("resident thread")
            .expect("resident round");
    }

    #[test]
    fn adapter_use_observer_reports_only_the_first_use_in_scope() {
        let pool = AdapterPool::new();
        let entry = test_entry("oneshot");
        let vault = test_vault("oneshot");
        let spawns = Arc::new(AtomicUsize::new(0));

        let (observer, seen) = observed_handle();
        let first_pid = with_adapter_use_observer(observer, || {
            let first = pool
                .with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                    Ok(adapter.process_id())
                })
                .expect("first round");
            // A nested/subsequent use inside the same scope (e.g. a tool that
            // chats through an adapter) must not overwrite the reported handle.
            pool.with_spawner(&entry, &vault, counting_spawner(&spawns), |adapter| {
                Ok(adapter.process_id())
            })
            .expect("second round");
            first
        });

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "observer must be one-shot per scope");
        assert_eq!(seen[0].pid(), first_pid);
    }
}
