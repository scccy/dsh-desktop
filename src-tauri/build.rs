fn main() {
    // `tauri-build` only watches the config and the capability files. Without
    // this line a replaced icon would keep the stale Windows resource (task
    // bar, Alt+Tab, Explorer) until some unrelated change forced a rebuild.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build();
}
