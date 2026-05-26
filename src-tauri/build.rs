fn main() {
    configure_resource_compiler();
    disable_mingw_default_manifest();

    println!("cargo:rerun-if-changed=windows-app-manifest.xml");

    let windows_attributes = tauri_build::WindowsAttributes::new()
        .app_manifest(include_str!("windows-app-manifest.xml"));
    let attributes = tauri_build::Attributes::new().windows_attributes(windows_attributes);

    tauri_build::try_build(attributes).expect("failed to run tauri build script");
}

fn disable_mingw_default_manifest() {
    #[cfg(all(windows, target_env = "gnu"))]
    {
        use std::{env, fs};

        let out_dir =
            env::var_os("OUT_DIR").expect("OUT_DIR is required for the Tauri build script");

        let specs_path = std::path::PathBuf::from(out_dir).join("no-default-manifest.specs");
        // MinGW GCC appends default-manifest.o in *endfile, which overrides the Tauri manifest.
        let specs = concat!(
            "*endfile:\n",
            "%{mdaz-ftz:crtfastmath.o%s;Ofast|ffast-math|funsafe-math-optimizations:",
            "%{!shared:%{!mno-daz-ftz:crtfastmath.o%s}}} ",
            "%{fvtable-verify=none:%s;fvtable-verify=preinit:vtv_end.o%s;",
            "fvtable-verify=std:vtv_end.o%s} crtend.o%s\n",
        );

        fs::write(&specs_path, specs).expect("failed to write MinGW specs override");
        println!("cargo:rustc-link-arg-bins=-specs={}", specs_path.display());
    }
}

fn configure_resource_compiler() {
    #[cfg(all(windows, target_env = "gnu"))]
    {
        use std::{
            env, fs,
            path::{Path, PathBuf},
        };

        let llvm_windres = match find_on_path("llvm-windres.exe") {
            Some(value) => value,
            None => return,
        };

        let out_dir = match env::var_os("OUT_DIR") {
            Some(value) => PathBuf::from(value),
            None => return,
        };

        let tools_dir = out_dir.join("resource-tools");
        if fs::create_dir_all(&tools_dir).is_err() {
            return;
        }

        let windres = tools_dir.join("windres.exe");
        if fs::copy(&llvm_windres, &windres).is_err() {
            return;
        }

        let llvm_tools_dir = llvm_windres.parent().map(Path::to_path_buf);
        prepend_paths(std::iter::once(tools_dir).chain(llvm_tools_dir));

        fn find_on_path(file_name: &str) -> Option<PathBuf> {
            let path = env::var_os("PATH")?;
            env::split_paths(&path)
                .map(|path| path.join(file_name))
                .find(|candidate| candidate.is_file())
        }

        fn prepend_paths(paths_to_prepend: impl IntoIterator<Item = PathBuf>) {
            let current_path = env::var_os("PATH").unwrap_or_default();
            let paths = paths_to_prepend
                .into_iter()
                .chain(env::split_paths(&current_path));

            if let Ok(joined_path) = env::join_paths(paths) {
                env::set_var("PATH", joined_path);
            }
        }
    }
}
