//! Scans a plugins directory for adapter manifests.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use mothership_adapter_host::AdapterRegistry;

#[test]
fn scans_and_finds_adapter_manifest() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mship_adapters_{stamp}"));
    let adapter_dir = dir.join("echo");
    fs::create_dir_all(&adapter_dir).expect("create adapter dir");
    fs::write(
        adapter_dir.join("adapter.json"),
        r#"{"provider_id":"echo","provider_label":"Echo Provider","program":"echo_adapter.exe"}"#,
    )
    .expect("write manifest");

    let registry = AdapterRegistry::scan(&dir);

    let entry = registry.find("echo").expect("echo adapter found");
    assert_eq!(entry.provider_label, "Echo Provider");
    assert_eq!(entry.program, adapter_dir.join("echo_adapter.exe"));
    assert!(registry.find("missing").is_none());

    let _ = fs::remove_dir_all(&dir);
}
