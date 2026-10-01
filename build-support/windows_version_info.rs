// Shared by the four MultiCore build scripts via `include!`. Builds the Windows
// VERSIONINFO resource so Task Manager shows a readable name per process.

/// Reads `version` from the desktop crate's `[package]` table (the app version
/// that every MultiCore executable shares).
#[allow(dead_code)]
fn multicore_app_version(manifest_dir: &std::path::Path) -> String {
    let desktop_manifest = manifest_dir.join("../../apps/multicore-desktop/Cargo.toml");
    println!("cargo:rerun-if-changed={}", desktop_manifest.display());
    let desktop = std::fs::read_to_string(&desktop_manifest).expect("desktop Cargo.toml");
    desktop
        .lines()
        .skip_while(|line| line.trim() != "[package]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .expect("desktop [package] version")
        .to_owned()
}

/// Returns an `.rc` fragment (UTF-8, `#pragma code_page(65001)`) with a
/// VERSIONINFO block. It never declares RT_MANIFEST.
#[allow(dead_code)]
fn windows_version_info(version: &str, description: &str, original_filename: &str) -> String {
    let mut numbers = version
        .split(['-', '+'])
        .next()
        .unwrap_or("0")
        .split('.')
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let numeric = [
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
        0,
    ];
    let comma = format!("{},{},{},{}", numeric[0], numeric[1], numeric[2], numeric[3]);
    let internal_name = original_filename.trim_end_matches(".exe");
    format!(
        r#"#pragma code_page(65001)
1 VERSIONINFO
FILEVERSION {comma}
PRODUCTVERSION {comma}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "MULTICORE"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{internal_name}"
      VALUE "LegalCopyright", "© 2026 MULTICORE contributors"
      VALUE "OriginalFilename", "{original_filename}"
      VALUE "ProductName", "MULTICORE"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
