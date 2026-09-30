include!("../../build-support/windows_version_info.rs");

fn main() {
    println!("cargo:rerun-if-changed=../../build-support/windows_version_info.rs");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-env-changed=MULTICORE_UPDATE_REPOSITORY");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    // The service compares update versions with the app's own version, which lives in the
    // desktop crate (the host crate keeps 0.1.0).
    let version = multicore_app_version(&manifest_dir);
    println!("cargo:rustc-env=MULTICORE_APP_VERSION={version}");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let manifest_path = manifest_dir.join("app.manifest");
    let resource_path = output_dir.join("multicore-core-host.rc");
    let mut resource_source = format!(
        "1 24 \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
    );
    resource_source.push_str(&windows_version_info(
        &version,
        "MultiCore — служба ядер",
        "multicore-core-host.exe",
    ));
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore core-host Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore core-host Windows manifest");
}
