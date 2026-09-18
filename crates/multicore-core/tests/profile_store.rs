use std::{fs, path::PathBuf, sync::Arc};

use multicore_core::{MAX_PROFILE_INDEX_BYTES, MAX_PROFILES, ProfileError, ProfileStore, Snapshot};

#[cfg(windows)]
use std::path::Path;

fn snapshot(source: &str, name: &str) -> Arc<Snapshot> {
    let mihomo = format!(
        "proxies:\n  - name: {name}\n    type: socks5\n    server: 127.0.0.1\n    port: 31001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [{name}]\nrules:\n  - MATCH,Proxy\n"
    );
    Arc::new(
        Snapshot::parse(mihomo.as_bytes(), br#"{"outbounds":[]}"#)
            .unwrap()
            .with_subscription_source(source, None, 1_700_000_000)
            .unwrap(),
    )
}

struct TempDirectory(tempfile::TempDir);

impl TempDirectory {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn profiles(&self) -> PathBuf {
        self.0.path().join("profiles")
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[test]
fn ids_are_unique_lowercase_v4_uuids_and_index_keeps_urls_secret() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    let first = store
        .create_profile(snapshot("https://one.invalid/private-a", "One"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://two.invalid/private-b", "Two"))
        .unwrap();

    assert_ne!(first, second);
    for id in [&first, &second] {
        let text = id.as_str();
        assert_eq!(text.len(), 36);
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(text, text.to_ascii_lowercase());
    }

    store.activate(&first).unwrap();
    assert_eq!(store.active_id(), Some(first.clone()));
    let index = fs::read_to_string(root.profiles().join("index.json")).unwrap();
    assert!(!index.contains("https://"));
    assert!(!index.contains("private-a"));
    assert!(!index.contains("private-b"));
    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries.iter().filter(|profile| profile.active).count(), 1);
    assert_eq!(index.matches("active_id").count(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.profiles().join("index.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}

#[cfg(windows)]
#[test]
fn index_keeps_owner_only_protected_acl_across_atomic_replacements() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let index = profiles.join("index.json");
    let store = ProfileStore::open(&profiles, None).unwrap();

    let first = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    assert_protected_single_principal_dacl(&index);

    store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();
    assert_protected_single_principal_dacl(&index);

    store.activate(&first).unwrap();
    assert_protected_single_principal_dacl(&index);
}

#[test]
fn seventeenth_profile_is_rejected_without_changing_index() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    for index in 0..MAX_PROFILES {
        store
            .create_profile(snapshot(
                &format!("https://{index}.invalid/sub"),
                &format!("Node{index}"),
            ))
            .unwrap();
    }
    let before = fs::read(root.profiles().join("index.json")).unwrap();
    assert!(matches!(
        store.create_profile(snapshot("https://overflow.invalid/sub", "Overflow")),
        Err(ProfileError::LimitReached)
    ));
    assert_eq!(
        fs::read(root.profiles().join("index.json")).unwrap(),
        before
    );
}

#[test]
fn second_store_instance_is_rejected_until_lifetime_lock_is_released() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let first = ProfileStore::open(&profiles, None).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        ProfileStore::open(&profiles, None),
        Err(ProfileError::Locked)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    drop(first);
    assert!(ProfileStore::open(&profiles, None).is_ok());
}

#[test]
fn oversized_index_is_rejected_before_deserialization() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(
        profiles.join("index.json"),
        vec![b' '; MAX_PROFILE_INDEX_BYTES as usize + 1],
    )
    .unwrap();
    assert!(matches!(
        ProfileStore::open(profiles, None),
        Err(ProfileError::InvalidIndex(reason)) if reason.contains("size limit")
    ));
}

#[test]
fn create_replace_activate_delete_and_exact_lookup_stay_isolated() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let first = store
        .create_profile(snapshot("https://same.invalid/sub?a=1", "First"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://same.invalid/sub?a=2", "Second"))
        .unwrap();

    assert_eq!(
        store
            .find_exact_source("https://same.invalid/sub?a=1")
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        store.find_exact_source("https://same.invalid/sub").unwrap(),
        None
    );
    assert_eq!(store.active_id(), Some(second.clone()));
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );

    let generation = store
        .replace_profile(
            &first,
            snapshot("https://same.invalid/sub?a=1", "FirstUpdated"),
        )
        .unwrap();
    assert_eq!(generation, 2);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );
    assert_eq!(
        store.activate(&first).unwrap().mihomo.proxy_names(),
        &["FirstUpdated"]
    );

    assert_eq!(store.delete(&first).unwrap(), Some(second.clone()));
    assert_eq!(store.active_id(), Some(second.clone()));
    assert!(!profiles.join(first.as_str()).exists());
    assert!(profiles.join(second.as_str()).exists());
    assert_eq!(store.delete(&second).unwrap(), None);
    assert_eq!(store.active_id(), None);
}

#[test]
fn corrupt_child_is_unavailable_and_failed_activation_keeps_index_active() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let good = store
        .create_profile(snapshot("https://good.invalid/sub", "Good"))
        .unwrap();
    let corrupt = store
        .create_profile(snapshot("https://bad.invalid/sub", "Bad"))
        .unwrap();
    store.activate(&good).unwrap();
    drop(store);

    let generation = profiles
        .join(corrupt.as_str())
        .join("snapshot-00000000000000000001");
    fs::write(generation.join("xray.json"), b"{broken").unwrap();

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    let summaries = reopened.list().unwrap();
    assert!(
        summaries
            .iter()
            .any(|item| item.id == good && item.available)
    );
    assert!(
        summaries
            .iter()
            .any(|item| item.id == corrupt && !item.available)
    );
    let before = fs::read(profiles.join("index.json")).unwrap();
    assert!(matches!(
        reopened.activate(&corrupt),
        Err(ProfileError::Unavailable(_))
    ));
    assert_eq!(reopened.active_id(), Some(good));
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
}

#[test]
fn failed_replace_preserves_last_good_snapshot_and_active_index() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://stable.invalid/sub", "Stable"))
        .unwrap();
    let before = fs::read(profiles.join("index.json")).unwrap();
    let blocked_staging = profiles.join(id.as_str()).join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked_staging).unwrap();

    assert!(
        store
            .replace_profile(&id, snapshot("https://stable.invalid/sub", "Rejected"))
            .is_err()
    );
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert_eq!(
        reopened
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
}

#[test]
fn invalid_deserialized_id_is_rejected_before_any_child_path_is_used() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(
        profiles.join("index.json"),
        br#"{"schema_version":1,"profiles":[{"id":"../escape","created_at_unix":1}],"active_id":"../escape"}"#,
    )
    .unwrap();

    assert!(matches!(
        ProfileStore::open(profiles, None),
        Err(ProfileError::InvalidIndex(_))
    ));
}

#[test]
fn malformed_empty_and_active_states_are_rejected_in_both_directions() {
    const ID: &str = "8a7f10a1-6ea8-4f65-8f15-711a184019d8";
    let cases = [
        format!(r#"{{"schema_version":1,"profiles":[],"active_id":"{ID}"}}"#),
        format!(
            r#"{{"schema_version":1,"profiles":[{{"id":"{ID}","created_at_unix":1}}],"active_id":null}}"#
        ),
    ];
    for (case, contents) in cases.into_iter().enumerate() {
        let root = TempDirectory::new();
        let profiles = root.path().join(format!("profiles-{case}"));
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("index.json"), contents).unwrap();
        assert!(matches!(
            ProfileStore::open(profiles, None),
            Err(ProfileError::InvalidIndex(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn delete_rejects_symlink_child_without_touching_external_target_or_resurrecting() {
    use std::os::unix::fs::symlink;

    hostile_cleanup_target_is_not_followed(|target, link| symlink(target, link).unwrap());
}

#[cfg(windows)]
#[test]
fn delete_rejects_junction_child_without_touching_external_target_or_resurrecting() {
    use std::process::Command;

    hostile_cleanup_target_is_not_followed(|target, link| {
        let output = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
}

#[cfg(any(unix, windows))]
fn hostile_cleanup_target_is_not_followed(create_link: impl FnOnce(&PathBuf, &PathBuf)) {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let external = root.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("sentinel.txt"), b"do not remove").unwrap();

    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://hostile.invalid/sub", "Hostile"))
        .unwrap();
    let child = profiles.join(id.as_str());
    let retired = root.path().join("retired-real-profile");
    fs::rename(&child, &retired).unwrap();
    create_link(&external, &child);

    assert!(matches!(
        store.delete(&id),
        Err(ProfileError::CleanupDebt { .. })
    ));
    assert_eq!(
        fs::read(external.join("sentinel.txt")).unwrap(),
        b"do not remove"
    );
    assert!(child.exists());
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert!(reopened.list().unwrap().is_empty());
    assert_eq!(reopened.active_id(), None);

    fs::remove_dir(&child).unwrap();
}

#[cfg(windows)]
fn assert_protected_single_principal_dacl(path: &Path) {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, OBJECT_INHERIT_ACE,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "failed to inspect {}",
        path.display()
    );
    assert!(!dacl.is_null());

    let mut control = 0_u16;
    let mut revision = 0_u32;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0, "{}", path.display());

    let mut size = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(
        size.AceCount,
        1,
        "unexpected ACL entries for {}",
        path.display()
    );

    let mut raw_ace = ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut raw_ace) }, 0);
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(ace.Header.AceType, 0, "ACE must be ACCESS_ALLOWED");
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);
    assert_eq!(
        ace.Header.AceFlags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8,
        0,
        "index file ACE must not be inheritable"
    );

    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut bytes_needed = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed);
    }
    assert_ne!(bytes_needed, 0);
    let word_size = std::mem::size_of::<usize>();
    let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast(),
                bytes_needed,
                &mut bytes_needed,
            )
        },
        0
    );
    let token_user = unsafe { &*token_info.as_ptr().cast::<TOKEN_USER>() };
    let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    assert_ne!(unsafe { EqualSid(ace_sid, token_user.User.Sid) }, 0);

    unsafe { CloseHandle(token) };
    unsafe { LocalFree(descriptor.cast()) };
}
