fn main() {
    // Listing the commands makes them permission-gated: only windows named in capabilities/ can call them,
    // so the Lenovo sign-in window (a remote site) can't.
    let cmds = tauri_build::AppManifest::new().commands(&["status", "disks", "act", "open_url"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(cmds)).expect("tauri build script");
}
