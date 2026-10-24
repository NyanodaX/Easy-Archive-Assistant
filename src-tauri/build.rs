fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "choose_path",
            "choose_paths",
            "plan_operation",
            "execute_plan",
            "cancel_task",
        ]),
    ))
    .expect("desktop configuration must be valid");
}
