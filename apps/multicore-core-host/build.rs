fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-env-changed=MULTICORE_UPDATE_REPOSITORY");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    // The service compares update versions with the app's own version, which lives in the
    // desktop crate (the host crate keeps 0.1.0).
    let desktop_manifest = manifest_dir.join("../multicore-desktop/Cargo.toml");
    println!("cargo:rerun-if-changed={}", desktop_manifest.display());
    let desktop = std::fs::read_to_string(&desktop_manifest).expect("desktop Cargo.toml");
    let version = desktop
        .lines()
        .skip_while(|line| line.trim() != "[package]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .expect("desktop [package] version");
    println!("cargo:rustc-env=MULTICORE_APP_VERSION={version}");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let manifest_path = manifest_dir.join("app.manifest");
    let resource_path = output_dir.join("multicore-core-host.rc");
    let resource_source = format!(
        "1 24 \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
    );
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore core-host Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore core-host Windows manifest");
}
