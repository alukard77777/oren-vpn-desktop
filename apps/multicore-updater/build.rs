include!("../../build-support/windows_version_info.rs");

fn main() {
    println!("cargo:rerun-if-changed=windows-resource.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=../../build-support/windows_version_info.rs");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let manifest_dir = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
        );
        let output_dir =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
        // windows-resource.rc keeps the RT_MANIFEST #1 line; VERSIONINFO is appended.
        let manifest_path = manifest_dir.join("app.manifest");
        let mut resource_source = std::fs::read_to_string(manifest_dir.join("windows-resource.rc"))
            .expect("failed to read updater windows-resource.rc")
            .replace(
                "\"app.manifest\"",
                &format!(
                    "\"{}\"",
                    manifest_path.display().to_string().replace('\\', "/")
                ),
            );
        resource_source.push('\n');
        resource_source.push_str(&windows_version_info(
            &multicore_app_version(&manifest_dir),
            "MULTICORE — обновление",
            "multicore-apply.exe",
        ));
        let resource_path = output_dir.join("multicore-updater.rc");
        std::fs::write(&resource_path, resource_source)
            .expect("failed to create updater Windows resource source");
        embed_resource::compile(&resource_path, embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed updater Windows manifest");
    }
}
