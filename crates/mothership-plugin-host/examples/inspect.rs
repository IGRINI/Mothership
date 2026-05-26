//! Prints a component's imports/exports so we know whether the host linker must
//! provide WASI. Usage: cargo run -p mothership-plugin-host --example inspect -- <component.wasm>

use wasmtime::component::Component;
use wasmtime::Engine;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: inspect <component.wasm>");
    let engine = Engine::default();
    let bytes = std::fs::read(&path)?;
    let component = Component::new(&engine, &bytes)?;
    let ty = component.component_type();

    let mut any_import = false;
    for (name, _item) in ty.imports(&engine) {
        println!("import: {name}");
        any_import = true;
    }
    if !any_import {
        println!("(no imports)");
    }
    for (name, _item) in ty.exports(&engine) {
        println!("export: {name}");
    }
    Ok(())
}
