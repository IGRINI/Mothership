# Cross-Platform Pattern

How Core stays completely unaware of the operating system while still getting per-OS
behavior. This is the same *Hexagonal Core* idea already in
[`../ARCHITECTURE.md`](../ARCHITECTURE.md), stated precisely for the platform axis.

## The principle

Ports & adapters / dependency inversion. The trait is the **port**; each per-OS
implementation is an **adapter**. Core depends on the port, not on any platform.

The Rust twist vs. runtime DI in Java/C#: the platform adapter is selected at
**compile time** via `#[cfg]`, because the OS is fixed per binary. You never need a
Windows adapter inside a Linux build — and it should not even compile there (it pulls
`windows-sys`).

## Direction of dependency — the one thing to get right

Wrong instinct: "each OS provides its own API." If the platform shapes the contract,
the trait grows methods like `job_object_handle()` / `cgroup_path()` that only mean
something on one OS — and Core has to know which OS it is on to call the right one.
Platform-awareness has leaked straight back in.

Correct: **Core defines one neutral contract; adapters conform to it.**

```text
        CORE defines ONE neutral contract (the port)
         ↑                                       ↑
   core depends on port           adapters depend on port (implement it)

   core   does NOT depend on adapters
   adapters do NOT depend on core
   only the binary (composition root) knows both — it wires them via #[cfg]
```

The contract speaks **Core's vocabulary** — "spawn a tool, kill its whole tree, stream
its output" — never the platform's ("create job object", "killpg", "kqueue").

## Two axes of variation

Do not conflate these; mixing them is the trap.

```text
platform (Windows / Linux / macOS)   -> known at COMPILE time -> #[cfg]
capability / policy / mock           -> known at RUNTIME       -> dyn / enum
```

- Platform → `#[cfg]` selection. Don't carry all three OS impls in the binary and
  dispatch the OS at runtime.
- Runtime choices (Linux: cgroup available or not; tests: mock vs real; future:
  remote-exec) → that's where a `dyn` port object or an enum earns its place.

## Crate layout

```text
crates/
  ports/      trait defs + neutral DTOs only          (no platform deps)
  core/       application logic; depends on ports      (no platform deps)
  adapters/   per-OS impls; depend on ports + platform crates, behind #[cfg]
  app/        composition root: depends on all; wires the adapter via #[cfg]
```

`ports` is a separate crate only to break the dependency cycle (adapters can't depend on
a heavy `core`). Conceptually the contract still belongs to Core's needs.

Composition root — the **only** place the OS is chosen:

```rust
pub fn platform_sandbox() -> Arc<dyn ProcessSandbox> {
    #[cfg(windows)]       return Arc::new(adapters::windows::JobObjectSandbox::new());
    #[cfg(target_os="linux")] return Arc::new(adapters::linux::new());
    #[cfg(target_os="macos")] return Arc::new(adapters::macos::new());
}
```

`ToolSupervisor` receives `Arc<dyn ProcessSandbox>` and knows nothing about
`JobObject` / `killpg` / `kqueue`. In tests it gets a `MockSandbox`.

## The leak test (verifiable)

```text
cargo tree -p mothership-core
```

Core's dependency tree must contain **zero** platform crates — no `windows-sys`,
no `nix`, no `cgroups-rs`. If one appears, the abstraction has leaked; go find where.
Core must build identically on all three OSes.

## Don't over-port

A port is justified only where there is **real** platform divergence or a need for a
seam (test mock, future swap). Realistic port list here:

```text
ProcessSandbox   yes  — spawn/kill (see PROCESS_SANDBOX.md)
IpcTransport     later — stdio now, interprocess later (see OUTPUT_AND_IPC.md)
SecretStore      likely — Win Credential Manager / macOS Keychain / Linux Secret Service
```

Do **not** invent your own port for these — mature cross-platform crates already cover
them:

```text
file watching   -> notify
config/data dirs -> directories
secrets          -> keyring   (if it fits; else a thin SecretStore port)
```

Things that are the same on every OS (tokio, serde, the agent state machine) are not
ports — they're just shared code in Core.

## CI matrix is mandatory

`#[cfg]`-gated code that no one compiles is code that silently rots: a Linux developer
never builds the Windows branch until it breaks in a release. Build **and test** every
target in CI:

```text
windows-latest  |  ubuntu-latest  |  macos-latest      ->  cargo build && cargo test
```
