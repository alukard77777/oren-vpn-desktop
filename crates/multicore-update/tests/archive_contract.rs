use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use multicore_release::{
    MAX_ARCHIVE_ENTRIES, MAX_EXPANDED_BYTES, WindowsArch, archive_entry_unix_mode,
    extract_verified_bundle, extract_verified_bundle_for_arch, extract_verified_bundle_with_limit,
    sha256_bytes,
};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

/// The other Windows flavour than this build's.
fn foreign_arch() -> WindowsArch {
    match WindowsArch::CURRENT {
        WindowsArch::X64 => WindowsArch::X86,
        WindowsArch::X86 => WindowsArch::X64,
    }
}

fn fake_pe_for(arch: WindowsArch) -> Vec<u8> {
    // PE32+ carries a 0xf0-byte optional header, PE32 a 0xe0-byte one.
    let optional_size: u16 = match arch {
        WindowsArch::X64 => 0x00f0,
        WindowsArch::X86 => 0x00e0,
    };
    let mut bytes = vec![0_u8; 512];
    bytes[0..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&arch.pe_machine().to_le_bytes());
    bytes[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
    bytes[0x94..0x96].copy_from_slice(&optional_size.to_le_bytes());
    bytes[0x96..0x98].copy_from_slice(&0x0002_u16.to_le_bytes());
    bytes[0x98..0x9a].copy_from_slice(&arch.pe_optional_magic().to_le_bytes());
    bytes
}

fn fake_pe() -> Vec<u8> {
    fake_pe_for(WindowsArch::CURRENT)
}

fn required_files_for(arch: WindowsArch) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("MultiCore.exe".into(), fake_pe_for(arch)),
        ("README.md".into(), b"readme".to_vec()),
        ("THIRD_PARTY_NOTICES.md".into(), b"notices".to_vec()),
        ("cores/mihomo.exe".into(), fake_pe_for(arch)),
        ("cores/xray.exe".into(), fake_pe_for(arch)),
        ("licenses/Xray-core-MPL-2.0.txt".into(), b"mpl".to_vec()),
        ("licenses/mihomo-GPL-3.0.txt".into(), b"gpl".to_vec()),
        ("runtime/multicore-core-host.exe".into(), fake_pe_for(arch)),
        ("runtime/multicore-daemon.exe".into(), fake_pe_for(arch)),
        ("runtime/multicore-updater.exe".into(), fake_pe_for(arch)),
        ("versions.json".into(), b"{}".to_vec()),
    ])
}

fn required_files() -> BTreeMap<String, Vec<u8>> {
    required_files_for(WindowsArch::CURRENT)
}

fn write_bundle(path: &Path, files: &BTreeMap<String, Vec<u8>>, sums: &BTreeMap<String, Vec<u8>>) {
    let file = File::create(path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        archive.start_file(name, options).unwrap();
        archive.write_all(bytes).unwrap();
    }
    let checksum_text = sums
        .iter()
        .map(|(name, bytes)| format!("{} *{}\n", sha256_bytes(bytes), name))
        .collect::<String>();
    archive.start_file("SHA256SUMS.txt", options).unwrap();
    archive.write_all(checksum_text.as_bytes()).unwrap();
    archive.finish().unwrap();
}

fn mark_central_entry_as_unix_symlink(path: &Path, name: &[u8]) {
    let mut bytes = std::fs::read(path).unwrap();
    let signature = [0x50, 0x4b, 0x01, 0x02];
    let mut found = false;
    for offset in 0..bytes.len().saturating_sub(46) {
        if bytes[offset..offset + 4] != signature {
            continue;
        }
        let name_len =
            u16::from_le_bytes(bytes[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(bytes[offset + 30..offset + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(bytes[offset + 32..offset + 34].try_into().unwrap()) as usize;
        let name_start = offset + 46;
        if name_start + name_len > bytes.len() || &bytes[name_start..name_start + name_len] != name
        {
            continue;
        }
        bytes[offset + 4..offset + 6].copy_from_slice(&[0x14, 0x03]);
        bytes[offset + 38..offset + 42].copy_from_slice(&(0o120777_u32 << 16).to_le_bytes());
        assert!(extra_len <= bytes.len() && comment_len <= bytes.len());
        found = true;
        break;
    }
    assert!(found, "central directory entry not found");
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn valid_bundle_extracts_and_verifies_every_file() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let files = required_files();
    write_bundle(&archive, &files, &files);

    let destination = temp.path().join("out");
    let inventory = extract_verified_bundle(&archive, &destination).unwrap();
    assert_eq!(inventory.files(), 12);
    assert_eq!(
        std::fs::read(destination.join("MultiCore.exe")).unwrap(),
        fake_pe()
    );
}

#[test]
fn each_flavour_accepts_only_its_own_executables() {
    for (bundle_arch, client_arch) in [
        (WindowsArch::X64, WindowsArch::X64),
        (WindowsArch::X86, WindowsArch::X86),
        (WindowsArch::X64, WindowsArch::X86),
        (WindowsArch::X86, WindowsArch::X64),
    ] {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("bundle.zip");
        let files = required_files_for(bundle_arch);
        write_bundle(&archive, &files, &files);
        let destination = temp.path().join("out");
        let result = extract_verified_bundle_for_arch(&archive, &destination, client_arch);
        if bundle_arch == client_arch {
            assert_eq!(result.unwrap().files(), 12);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(client_arch.pe_description()),
                "{bundle_arch:?} bundle for {client_arch:?}: {error}"
            );
            assert!(!destination.exists(), "a rejected bundle must be removed");
        }
    }
}

#[test]
fn this_build_rejects_the_other_flavours_bundle() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let files = required_files_for(foreign_arch());
    write_bundle(&archive, &files, &files);
    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(WindowsArch::CURRENT.pe_description()),
        "{error}"
    );
}

#[test]
fn traversal_and_unlisted_files_are_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.insert("../escape.txt".into(), b"escape".to_vec());
    let sums = required_files();
    write_bundle(&archive, &files, &sums);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(error.to_string().contains("unsafe archive path"));
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn absolute_and_normalized_duplicate_paths_are_rejected() {
    for name in ["/escape.txt", "foo/../escape.txt"] {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("bundle.zip");
        let mut files = required_files();
        files.insert(name.into(), b"escape".to_vec());
        write_bundle(&archive, &files, &files);
        let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
        assert!(error.to_string().contains("unsafe archive path"));
    }

    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.insert("dir/file".into(), b"one".to_vec());
    files.insert("dir//file".into(), b"two".to_vec());
    write_bundle(&archive, &files, &files);
    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(error.to_string().contains("duplicate archive path"));
}

#[test]
fn symlink_entries_are_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let files = required_files();
    let file = File::create(&archive).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &files {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    let options = SimpleFileOptions::default().unix_permissions(0o120777);
    writer.start_file("link", options).unwrap();
    writer.write_all(b"MultiCore.exe").unwrap();
    let sums = files
        .iter()
        .map(|(name, bytes)| format!("{} *{}\n", sha256_bytes(bytes), name))
        .chain(std::iter::once(format!(
            "{} *link\n",
            sha256_bytes(b"MultiCore.exe")
        )))
        .collect::<String>();
    writer
        .start_file("SHA256SUMS.txt", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(sums.as_bytes()).unwrap();
    writer.finish().unwrap();
    mark_central_entry_as_unix_symlink(&archive, b"link");
    let mut reopened = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap();
    let link = reopened.by_name("link").unwrap();
    assert_eq!(archive_entry_unix_mode(&link).unwrap() & 0o170000, 0o120000);
    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(
        error.to_string().contains("archive links are not allowed"),
        "{error}"
    );
}

#[test]
fn unlisted_extra_and_entry_limit_are_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.insert("extra.txt".into(), b"extra".to_vec());
    write_bundle(&archive, &files, &required_files());
    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("checksum inventory does not match")
    );

    let archive = temp.path().join("too-many.zip");
    let mut files = required_files();
    for index in 0..(MAX_ARCHIVE_ENTRIES - files.len()) {
        files.insert(format!("extra/{index}.txt"), vec![index as u8]);
    }
    write_bundle(&archive, &files, &files);
    let error = extract_verified_bundle(&archive, &temp.path().join("too-many-out")).unwrap_err();
    assert!(error.to_string().contains("entry count is invalid"));
}

#[test]
fn expanded_size_limit_is_explicitly_guarded() {
    assert_eq!(MAX_EXPANDED_BYTES, 1024 * 1024 * 1024);
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let files = required_files();
    write_bundle(&archive, &files, &files);
    let destination = temp.path().join("out");
    let error = extract_verified_bundle_with_limit(&archive, &destination, 1).unwrap_err();
    assert!(error.to_string().contains("expanded archive is too large"));
    assert!(!destination.exists(), "failed extraction must clean up");
}

#[test]
fn malformed_or_missing_pe_and_hash_are_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bad-pe.zip");
    let mut files = required_files();
    files.insert("MultiCore.exe".into(), vec![0_u8; 256]);
    write_bundle(&archive, &files, &files);
    let error = extract_verified_bundle(&archive, &temp.path().join("bad-pe-out")).unwrap_err();
    assert!(error.to_string().contains("PE"), "{error}");

    let archive = temp.path().join("bad-hash.zip");
    let files = required_files();
    let mut sums = files.clone();
    sums.insert("README.md".into(), b"wrong".to_vec());
    write_bundle(&archive, &files, &sums);
    let error = extract_verified_bundle(&archive, &temp.path().join("bad-hash-out")).unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
}

#[test]
fn checksum_mismatch_is_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    let sums = files.clone();
    files.insert("README.md".into(), b"tampered".to_vec());
    write_bundle(&archive, &files, &sums);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
}

#[test]
fn missing_required_updater_is_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.remove("runtime/multicore-updater.exe");
    write_bundle(&archive, &files, &files);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required bundle file is missing")
    );
}

#[test]
fn missing_or_foreign_core_host_is_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("missing-core-host.zip");
    let mut files = required_files();
    files.remove("runtime/multicore-core-host.exe");
    write_bundle(&archive, &files, &files);

    let error = extract_verified_bundle(&archive, &temp.path().join("missing-out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required bundle file is missing: runtime/multicore-core-host.exe"),
        "{error}"
    );

    // The right machine with the other flavour's optional-header magic (PE32 vs PE32+).
    let archive = temp.path().join("foreign-magic-core-host.zip");
    let mut files = required_files();
    let mut foreign_magic = fake_pe();
    foreign_magic[0x98..0x9a].copy_from_slice(&foreign_arch().pe_optional_magic().to_le_bytes());
    files.insert("runtime/multicore-core-host.exe".into(), foreign_magic);
    write_bundle(&archive, &files, &files);

    let error = extract_verified_bundle(&archive, &temp.path().join("magic-out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(WindowsArch::CURRENT.pe_description()),
        "{error}"
    );

    // Only the core host built for the other flavour.
    let archive = temp.path().join("foreign-core-host.zip");
    let mut files = required_files();
    files.insert(
        "runtime/multicore-core-host.exe".into(),
        fake_pe_for(foreign_arch()),
    );
    write_bundle(&archive, &files, &files);
    let error = extract_verified_bundle(&archive, &temp.path().join("foreign-out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(WindowsArch::CURRENT.pe_description()),
        "{error}"
    );
}

#[test]
fn malformed_core_host_pe_structure_and_image_flags_are_rejected() {
    let cases = [
        ("zero-sections", {
            let mut pe = fake_pe();
            pe[0x86..0x88].copy_from_slice(&0_u16.to_le_bytes());
            pe
        }),
        ("truncated-section-table", {
            let mut pe = fake_pe();
            pe.truncate(0x188);
            pe
        }),
        ("not-executable", {
            let mut pe = fake_pe();
            pe[0x96..0x98].copy_from_slice(&0_u16.to_le_bytes());
            pe
        }),
        ("dll", {
            let mut pe = fake_pe();
            pe[0x96..0x98].copy_from_slice(&0x2002_u16.to_le_bytes());
            pe
        }),
    ];

    for (name, core_host) in cases {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join(format!("{name}.zip"));
        let mut files = required_files();
        files.insert("runtime/multicore-core-host.exe".into(), core_host);
        write_bundle(&archive, &files, &files);

        let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(WindowsArch::CURRENT.pe_description()),
            "{name}: {error}"
        );
    }
}

#[test]
#[ignore = "requires a locally built release ZIP named by MULTICORE_RELEASE_BUNDLE"]
fn built_release_bundle_matches_updater_contract() {
    let archive = std::env::var_os("MULTICORE_RELEASE_BUNDLE")
        .map(std::path::PathBuf::from)
        .expect("MULTICORE_RELEASE_BUNDLE must name the built release ZIP");
    // The x64 test binary also checks the x86 bundle: MULTICORE_RELEASE_BUNDLE_ARCH=x86.
    let arch = match std::env::var("MULTICORE_RELEASE_BUNDLE_ARCH").as_deref() {
        Ok("x86") => WindowsArch::X86,
        Ok("x64") => WindowsArch::X64,
        Ok(other) => panic!("unknown MULTICORE_RELEASE_BUNDLE_ARCH {other:?}"),
        Err(_) => WindowsArch::CURRENT,
    };
    let expected_name = arch.bundle_asset_name();
    assert_eq!(
        archive.file_name().and_then(|name| name.to_str()),
        Some(expected_name),
        "the {arch:?} release bundle must be named {expected_name}"
    );
    let temp = TempDir::new().unwrap();
    let destination = temp.path().join("release");

    let inventory = extract_verified_bundle_for_arch(&archive, &destination, arch).unwrap();

    // Every payload file is listed in SHA256SUMS.txt, which is itself in the
    // bundle; a fixed count broke once the dashboard (32 files) was bundled.
    let listed = std::fs::read_to_string(destination.join("SHA256SUMS.txt"))
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(inventory.files(), listed + 1);
    for required in [
        "MultiCore.exe",
        "runtime/multicore-core-host.exe",
        "dashboard/index.html",
    ] {
        assert!(destination.join(required).is_file(), "{required}");
    }
}
