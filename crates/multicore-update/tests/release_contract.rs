use multicore_release::{
    GitHubRelease, ReleaseDecision, Repository, Version, WINDOWS_ASSET_NAME,
    WINDOWS_INSTALLER_ASSET_NAME, WindowsArch,
};

#[test]
fn repository_accepts_one_owner_and_repository() {
    let repository = Repository::parse("Fatyzzz/multi-core").unwrap();
    assert_eq!(repository.owner(), "Fatyzzz");
    assert_eq!(repository.name(), "multi-core");

    for invalid in ["", "owner", "owner/repo/extra", "../repo", "owner repo/x"] {
        assert!(Repository::parse(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn stable_versions_compare_numerically() {
    assert!(Version::parse("v1.10.0").unwrap() > Version::parse("0.9.9").unwrap());
    assert!(Version::parse("1.2.3").is_ok());
    for invalid in ["1.2", "v1.2.3-beta.1", "latest", "1.2.3.4"] {
        assert!(Version::parse(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn release_requires_exact_asset_size_and_sha256_digest() {
    let body = br#"{
        "tag_name":"v0.2.0",
        "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
        "assets":[{
            "name":"multicore-windows-x64.zip",
            "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
            "size":1234,
            "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }]
    }"#;

    let release = GitHubRelease::parse_for_arch(body, WindowsArch::X64).unwrap();
    assert_eq!(release.version().to_string(), "0.2.0");
    assert_eq!(release.asset().size(), 1234);
    assert_eq!(release.asset().sha256_hex(), "a".repeat(64));
    assert!(matches!(
        release.decision(&Version::parse("0.1.0").unwrap()),
        ReleaseDecision::UpdateAvailable
    ));
    assert!(matches!(
        release.decision(&Version::parse("0.2.0").unwrap()),
        ReleaseDecision::Current
    ));
}

#[test]
fn release_rejects_missing_or_non_sha256_digest() {
    for digest in ["", "sha512:abcd", "sha256:abcd"] {
        let body = format!(
            r#"{{"tag_name":"v0.2.0","html_url":"https://github.com/o/r/releases/tag/v0.2.0","assets":[{{"name":"multicore-windows-x64.zip","browser_download_url":"https://github.com/o/r/releases/download/v0.2.0/multicore-windows-x64.zip","size":12,"digest":"{digest}"}}]}}"#
        );
        assert!(GitHubRelease::parse_for_arch(body.as_bytes(), WindowsArch::X64).is_err());
    }
}

#[test]
fn pinned_repository_rejects_cross_repository_release_urls() {
    let repository = Repository::parse("owner/repo").unwrap();
    let body = br#"{
        "tag_name":"v0.2.0",
        "html_url":"https://github.com/attacker/repo/releases/tag/v0.2.0",
        "assets":[{
            "name":"multicore-windows-x64.zip",
            "browser_download_url":"https://github.com/attacker/repo/releases/download/v0.2.0/multicore-windows-x64.zip",
            "size":12,
            "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }]
    }"#;
    assert!(GitHubRelease::parse_for_repository(&repository, body).is_err());
}

const MIXED_RELEASE: &[u8] = br#"{
    "tag_name":"v0.3.0",
    "html_url":"https://github.com/fatyzzz/multicore-releases/releases/tag/v0.3.0",
    "assets":[
        {
            "name":"multicore-windows-x64.zip",
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v0.3.0/multicore-windows-x64.zip",
            "size":1234,
            "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        {
            "name":"multicore-windows-x86.zip",
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v0.3.0/multicore-windows-x86.zip",
            "size":4321,
            "digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        },
        {
            "name":"multicore-windows-arm64.zip",
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v0.3.0/multicore-windows-arm64.zip",
            "size":5555,
            "digest":"sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        },
        {
            "name":"MultiCore-0.3.0-x86_64.AppImage",
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v0.3.0/MultiCore-0.3.0-x86_64.AppImage",
            "size":5678,
            "digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }
    ]
}"#;

#[test]
fn appimage_release_selects_the_versioned_linux_asset() {
    let repository = Repository::parse("fatyzzz/multicore-releases").unwrap();
    let release = GitHubRelease::parse_appimage_for_repository(&repository, MIXED_RELEASE).unwrap();
    assert_eq!(release.version().to_string(), "0.3.0");
    assert_eq!(release.asset().size(), 5678);
    assert_eq!(release.asset().sha256_hex(), "b".repeat(64));
    assert!(
        release
            .asset()
            .download_url()
            .ends_with("/MultiCore-0.3.0-x86_64.AppImage")
    );
    assert_eq!(
        multicore_release::appimage_asset_name(&release.version()),
        "MultiCore-0.3.0-x86_64.AppImage"
    );

    let windows = GitHubRelease::parse_for_repository(&repository, MIXED_RELEASE).unwrap();
    assert!(
        windows
            .asset()
            .download_url()
            .ends_with(&format!("/{WINDOWS_ASSET_NAME}"))
    );
}

/// Size and digest character of each flavour's bundle in `MIXED_RELEASE`.
fn mixed_release_bundle(arch: WindowsArch) -> (u64, char) {
    match arch {
        WindowsArch::X64 => (1234, 'a'),
        WindowsArch::X86 => (4321, 'c'),
        WindowsArch::Arm64 => (5555, 'e'),
    }
}

#[test]
fn each_windows_flavour_takes_only_its_own_bundle() {
    assert_eq!(
        WindowsArch::X64.bundle_asset_name(),
        "multicore-windows-x64.zip"
    );
    assert_eq!(
        WindowsArch::X86.bundle_asset_name(),
        "multicore-windows-x86.zip"
    );
    assert_eq!(
        WindowsArch::Arm64.bundle_asset_name(),
        "multicore-windows-arm64.zip"
    );
    for arch in WindowsArch::ALL {
        let (size, digest) = mixed_release_bundle(arch);
        let release = GitHubRelease::parse_for_arch(MIXED_RELEASE, arch).unwrap();
        assert_eq!(release.asset().size(), size, "{arch:?}");
        assert_eq!(
            release.asset().sha256_hex(),
            digest.to_string().repeat(64),
            "{arch:?}"
        );
        assert!(
            release
                .asset()
                .download_url()
                .ends_with(&format!("/{}", arch.bundle_asset_name())),
            "{arch:?}"
        );
    }

    // A release without one flavour's bundle offers that flavour no update rather than
    // another flavour's bundle, and still updates the others.
    for missing in WindowsArch::ALL {
        let without = String::from_utf8(MIXED_RELEASE.to_vec())
            .unwrap()
            .replace(missing.bundle_asset_name(), "multicore-windows-other.zip");
        for arch in WindowsArch::ALL {
            let parsed = GitHubRelease::parse_for_arch(without.as_bytes(), arch);
            if arch == missing {
                assert!(parsed.is_err(), "{arch:?} must not take another bundle");
            } else {
                assert_eq!(
                    parsed.unwrap().asset().size(),
                    mixed_release_bundle(arch).0,
                    "{arch:?} without {missing:?}"
                );
            }
        }
    }
}

#[test]
fn this_build_uses_the_assets_of_its_own_architecture() {
    let expected = if cfg!(target_arch = "x86") {
        WindowsArch::X86
    } else if cfg!(target_arch = "aarch64") {
        WindowsArch::Arm64
    } else {
        WindowsArch::X64
    };
    assert_eq!(WindowsArch::CURRENT, expected);
    assert_eq!(WINDOWS_ASSET_NAME, expected.bundle_asset_name());
    assert_eq!(
        WINDOWS_INSTALLER_ASSET_NAME,
        expected.installer_asset_name()
    );
    let release = GitHubRelease::parse(MIXED_RELEASE).unwrap();
    assert!(
        release
            .asset()
            .download_url()
            .ends_with(&format!("/{}", expected.bundle_asset_name()))
    );
    for foreign in WindowsArch::ALL
        .into_iter()
        .filter(|arch| *arch != expected)
    {
        assert!(
            !release
                .asset()
                .download_url()
                .ends_with(foreign.bundle_asset_name())
        );
    }
}

#[test]
fn pe_identity_of_each_windows_flavour() {
    assert_eq!(WindowsArch::X64.pe_machine(), 0x8664);
    assert_eq!(WindowsArch::X86.pe_machine(), 0x014c);
    assert_eq!(WindowsArch::Arm64.pe_machine(), 0xaa64);
    assert_eq!(WindowsArch::X64.pe_optional_magic(), 0x20b);
    assert_eq!(WindowsArch::X86.pe_optional_magic(), 0x10b);
    assert_eq!(WindowsArch::Arm64.pe_optional_magic(), 0x20b);
    assert_eq!(WindowsArch::Arm64.pe_optional_header_fixed_size(), 112);
    assert_eq!(WindowsArch::Arm64.pe_description(), "PE32+ ARM64");
    // Machine and description tell every pair of flavours apart.
    for a in WindowsArch::ALL {
        for b in WindowsArch::ALL.into_iter().filter(|b| *b != a) {
            assert_ne!(a.pe_machine(), b.pe_machine());
            assert!(!a.pe_description().contains(b.pe_description()));
            assert_ne!(a.bundle_asset_name(), b.bundle_asset_name());
            assert_ne!(a.installer_asset_name(), b.installer_asset_name());
        }
    }
}

#[test]
fn appimage_release_rejects_missing_or_foreign_assets() {
    let repository = Repository::parse("fatyzzz/multicore-releases").unwrap();
    let windows_only = String::from_utf8(MIXED_RELEASE.to_vec()).unwrap().replace(
        "MultiCore-0.3.0-x86_64.AppImage",
        "MultiCore-0.2.9-x86_64.AppImage",
    );
    assert!(
        GitHubRelease::parse_appimage_for_repository(&repository, windows_only.as_bytes()).is_err()
    );
    let other = Repository::parse("someone/else").unwrap();
    assert!(GitHubRelease::parse_appimage_for_repository(&other, MIXED_RELEASE).is_err());
}

#[test]
fn the_installer_asset_is_parsed_from_the_pinned_repository() {
    assert_eq!(
        WindowsArch::X64.installer_asset_name(),
        "MultiCore-Setup-x64.exe"
    );
    assert_eq!(
        WindowsArch::X86.installer_asset_name(),
        "MultiCore-Setup-x86.exe"
    );
    assert_eq!(
        WindowsArch::Arm64.installer_asset_name(),
        "MultiCore-Setup-arm64.exe"
    );
    let repository = Repository::parse("fatyzzz/multicore-releases").unwrap();
    let digest = "a".repeat(64);
    let x86_digest = "d".repeat(64);
    let arm64_digest = "f".repeat(64);
    let body = format!(
        r#"{{"tag_name":"v1.1.0","html_url":"https://github.com/fatyzzz/multicore-releases/releases/tag/v1.1.0",
            "assets":[{{"name":"MultiCore-Setup-x64.exe","size":1000,
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v1.1.0/MultiCore-Setup-x64.exe",
            "digest":"sha256:{digest}"}},
            {{"name":"MultiCore-Setup-x86.exe","size":900,
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v1.1.0/MultiCore-Setup-x86.exe",
            "digest":"sha256:{x86_digest}"}},
            {{"name":"MultiCore-Setup-arm64.exe","size":950,
            "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v1.1.0/MultiCore-Setup-arm64.exe",
            "digest":"sha256:{arm64_digest}"}}]}}"#
    );
    let x64 = GitHubRelease::parse_installer_for_arch(body.as_bytes(), WindowsArch::X64).unwrap();
    assert!(
        x64.asset()
            .download_url()
            .ends_with("/MultiCore-Setup-x64.exe")
    );
    assert_eq!(x64.asset().sha256_hex(), digest);
    let x86 = GitHubRelease::parse_installer_for_arch(body.as_bytes(), WindowsArch::X86).unwrap();
    assert!(
        x86.asset()
            .download_url()
            .ends_with("/MultiCore-Setup-x86.exe")
    );
    assert_eq!(x86.asset().sha256_hex(), x86_digest);
    let arm64 =
        GitHubRelease::parse_installer_for_arch(body.as_bytes(), WindowsArch::Arm64).unwrap();
    assert!(
        arm64
            .asset()
            .download_url()
            .ends_with("/MultiCore-Setup-arm64.exe")
    );
    assert_eq!(arm64.asset().sha256_hex(), arm64_digest);
    // Each flavour's installer only: without the ARM64 installer an ARM64 client gets none.
    let without_arm64 = body.replace("MultiCore-Setup-arm64.exe", "MultiCore-Setup-other.exe");
    assert!(
        GitHubRelease::parse_installer_for_arch(without_arm64.as_bytes(), WindowsArch::Arm64)
            .is_err()
    );
    assert!(
        GitHubRelease::parse_installer_for_arch(without_arm64.as_bytes(), WindowsArch::X64).is_ok()
    );
    let release =
        GitHubRelease::parse_installer_for_repository(&repository, body.as_bytes()).unwrap();
    assert!(
        release
            .asset()
            .download_url()
            .ends_with(&format!("/{WINDOWS_INSTALLER_ASSET_NAME}"))
    );
    let other = Repository::parse("someone/else").unwrap();
    assert!(GitHubRelease::parse_installer_for_repository(&other, body.as_bytes()).is_err());
    // The portable-zip parser must not pick the installer up, and vice versa.
    assert!(GitHubRelease::parse_for_repository(&repository, body.as_bytes()).is_err());
    assert!(GitHubRelease::parse_installer_for_repository(&repository, MIXED_RELEASE).is_err());
}
