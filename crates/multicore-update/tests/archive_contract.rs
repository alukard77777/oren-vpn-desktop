use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use multicore_release::{
    MAX_ARCHIVE_ENTRIES, MAX_EXPANDED_BYTES, archive_entry_unix_mode, extract_verified_bundle,
    extract_verified_bundle_with_limit, sha256_bytes,
};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

fn fake_pe() -> Vec<u8> {
    let mut bytes = vec![0_u8; 256];
    bytes[0..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
    bytes
}

fn required_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("MultiCore.exe".into(), fake_pe()),
        ("README.md".into(), b"readme".to_vec()),
        ("THIRD_PARTY_NOTICES.md".into(), b"notices".to_vec()),
        ("cores/mihomo.exe".into(), fake_pe()),
        ("cores/xray.exe".into(), fake_pe()),
        ("licenses/Xray-core-MPL-2.0.txt".into(), b"mpl".to_vec()),
        ("licenses/mihomo-GPL-3.0.txt".into(), b"gpl".to_vec()),
        ("runtime/multicore-daemon.exe".into(), fake_pe()),
        ("runtime/multicore-updater.exe".into(), fake_pe()),
        ("versions.json".into(), b"{}".to_vec()),
    ])
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
    assert_eq!(inventory.files(), 11);
    assert_eq!(
        std::fs::read(destination.join("MultiCore.exe")).unwrap(),
        fake_pe()
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

    let inventory = extract_verified_bundle_with_limit(
        &archive,
        &temp.path().join("clamped-out"),
        MAX_EXPANDED_BYTES + 1,
    )
    .unwrap();
    assert!(inventory.expanded_bytes() > 1);
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
