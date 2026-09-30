#![cfg(unix)]

#[path = "../src/runtime_host.rs"]
#[allow(dead_code)]
mod runtime_host;
#[path = "../src/unix_runtime_paths.rs"]
#[allow(dead_code)]
mod unix_runtime_paths;

use multicore_platform::elf::test_support::TestElf;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};
use unix_runtime_paths::{ResolveError, resolve_from, user_data_root};

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    host: PathBuf,
    cores: PathBuf,
    generation: PathBuf,
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("package");
    let runtime = package.join("runtime");
    let cores = package.join("cores");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&cores).unwrap();
    let host = runtime.join("multicore-core-host");
    TestElf::position_independent().write(&host);
    TestElf::executable().write(&cores.join("xray"));
    TestElf::executable().write(&cores.join("mihomo"));
    let data = temp.path().join("data").join("multicore");
    let generation = data.join("runtime").join("runtime-00000000000000000042");
    fs::create_dir_all(&generation).unwrap();
    for name in ["xray.json", "mihomo.yaml"] {
        let path = generation.join(name);
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    Fixture {
        _temp: temp,
        data,
        host,
        cores,
        generation,
    }
}

fn resolve(fixture: &Fixture) -> Result<unix_runtime_paths::ResolvedRuntime, ResolveError> {
    resolve_from(&fixture.data, &fixture.host, 42, uid())
}

fn replace_with_symlink(path: &Path) {
    let moved = path.with_extension("moved");
    fs::rename(path, &moved).unwrap();
    symlink(&moved, path).unwrap();
}

#[test]
fn exact_layout_resolves_to_canonical_contained_paths() {
    let fixture = fixture();
    let resolved = resolve(&fixture).unwrap();
    assert_eq!(
        resolved.paths.xray_binary,
        fixture.cores.join("xray").canonicalize().unwrap()
    );
    assert_eq!(
        resolved.paths.mihomo_config,
        fixture
            .generation
            .join("mihomo.yaml")
            .canonicalize()
            .unwrap()
    );
    assert_eq!(resolved.data_root, fixture.data.canonicalize().unwrap());
    resolved.verify_unchanged().unwrap();
}

#[test]
fn invalid_generation_and_host_layout_are_rejected() {
    let fixture = fixture();
    assert_eq!(
        resolve_from(&fixture.data, &fixture.host, 0, uid()).err(),
        Some(ResolveError::InvalidGeneration)
    );
    assert!(resolve_from(&fixture.data, &fixture.host, 43, uid()).is_err());
    let renamed = fixture.host.with_file_name("multicore-core-host.exe");
    fs::rename(&fixture.host, &renamed).unwrap();
    assert_eq!(
        resolve_from(&fixture.data, &renamed, 42, uid()).err(),
        Some(ResolveError::Layout)
    );
}

#[test]
fn missing_configs_and_cores_are_rejected() {
    for victim in ["config", "core"] {
        let fixture = fixture();
        let path = if victim == "config" {
            fixture.generation.join("xray.json")
        } else {
            fixture.cores.join("mihomo")
        };
        fs::remove_file(path).unwrap();
        assert_eq!(resolve(&fixture).err(), Some(ResolveError::Missing));
    }
}

#[test]
fn symlinked_components_are_rejected() {
    for victim in ["config", "generation", "core", "cores"] {
        let fixture = fixture();
        let path = match victim {
            "config" => fixture.generation.join("xray.json"),
            "generation" => fixture.generation.clone(),
            "core" => fixture.cores.join("xray"),
            _ => fixture.cores.clone(),
        };
        replace_with_symlink(&path);
        assert_eq!(
            resolve(&fixture).err(),
            Some(ResolveError::Symlink),
            "{victim}"
        );
    }
}

#[test]
fn group_or_world_writable_components_are_rejected() {
    for (victim, mode) in [("generation", 0o770), ("core", 0o775), ("config", 0o606)] {
        let fixture = fixture();
        let path = match victim {
            "generation" => fixture.generation.clone(),
            "core" => fixture.cores.join("xray"),
            _ => fixture.generation.join("mihomo.yaml"),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            resolve(&fixture).err(),
            Some(ResolveError::Ownership),
            "{victim}"
        );
    }
}

#[test]
fn components_owned_by_another_user_are_rejected() {
    let fixture = fixture();
    assert_eq!(
        resolve_from(&fixture.data, &fixture.host, 42, uid() + 1).err(),
        Some(ResolveError::Ownership)
    );
}

#[test]
fn non_elf_cores_are_rejected() {
    let fixture = fixture();
    let core = fixture.cores.join("mihomo");
    fs::write(&core, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&core, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(resolve(&fixture).err(), Some(ResolveError::InvalidElf));
}

#[test]
fn replaced_config_is_detected_before_start() {
    let fixture = fixture();
    let resolved = resolve(&fixture).unwrap();
    let config = fixture.generation.join("xray.json");
    let replacement = fixture.generation.join("replacement");
    fs::write(&replacement, b"{}").unwrap();
    fs::rename(&replacement, &config).unwrap();
    assert_eq!(
        resolved.verify_unchanged().err(),
        Some(ResolveError::Escape)
    );
}

#[test]
fn data_root_follows_the_peer_xdg_data_home_or_the_passwd_home() {
    let passwd = "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/zsh\n";
    assert_eq!(
        user_data_root(1000, passwd, b"PATH=/usr/bin\0XDG_DATA_HOME=/data/alice\0").unwrap(),
        PathBuf::from("/data/alice/multicore")
    );
    assert_eq!(
        user_data_root(1000, passwd, b"XDG_DATA_HOME=relative\0").unwrap(),
        PathBuf::from("/home/alice/.local/share/multicore")
    );
    assert_eq!(
        user_data_root(1000, passwd, b"").unwrap(),
        PathBuf::from("/home/alice/.local/share/multicore")
    );
    assert_eq!(
        user_data_root(1001, passwd, b"").err(),
        Some(ResolveError::Context)
    );
}

#[test]
fn dashboard_absent_leaves_dashboard_directory_none() {
    let fixture = fixture();
    let resolved = resolve(&fixture).unwrap();
    assert_eq!(resolved.paths.dashboard_directory, None);
}

#[test]
fn resolves_an_optional_dashboard_sibling_of_cores_with_no_windows_style_prefix() {
    // Mirrors apps/multicore-core-host/src/runtime_paths.rs's Windows dashboard resolution:
    // `<package>/dashboard`, a sibling of `cores/`, not `<package>/runtime/dashboard` (the host
    // executable's own directory, one level deeper). Also confirms the reasoning behind not
    // needing runtime_paths.rs's `strip_verbatim_prefix` here: Linux's `fs::canonicalize` never
    // adds a `\\?\`-style prefix, so the value this resolver hands back for Mihomo's SAFE_PATHS
    // already byte-for-byte matches the unprefixed `external-ui` path the daemon writes into the
    // generated Mihomo config (crates/multicore-daemon/src/main.rs) with no stripping needed.
    let fixture = fixture();
    let package = fixture.host.parent().unwrap().parent().unwrap();
    let dashboard = package.join("dashboard");
    fs::create_dir_all(&dashboard).unwrap();
    fs::write(dashboard.join("index.html"), b"<html></html>").unwrap();

    let resolved = resolve(&fixture).unwrap();
    let canonical = dashboard.canonicalize().unwrap();
    assert_eq!(resolved.paths.dashboard_directory, Some(canonical.clone()));
    assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
}

#[test]
fn dashboard_replaced_with_a_symlink_is_rejected_not_silently_treated_as_absent() {
    // Regression for a real bug: `dashboard_present` used to be gated on
    // `symlink_metadata(&dashboard).is_dir()`, but on Unix `is_dir()` is *always* false for a
    // symlink (`lstat` reports `S_IFLNK`, never `S_IFDIR`, no matter what the symlink points
    // to) - unlike Windows, where a directory reparse point keeps `FILE_ATTRIBUTE_DIRECTORY`
    // set. That made a dashboard directory swapped for a symlink look identical to a genuinely
    // absent dashboard: presence was false, the guard chain never ran, and resolution quietly
    // succeeded with `dashboard_directory: None` instead of failing closed the way the Windows
    // resolver's equivalent check does (`ResolveError::Reparse`, see
    // apps/multicore-core-host/src/runtime_paths.rs's `rejects_a_dashboard_directory_replaced_
    // with_a_reparse_point`). The symlink is created directly at the dashboard path (never a
    // real directory first) so this cannot be satisfied by anything that merely tracks whether
    // a directory once existed there.
    let fixture = fixture();
    let package = fixture.host.parent().unwrap().parent().unwrap();
    let outside = fixture.data.parent().unwrap().join("outside-dashboard");
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, package.join("dashboard")).unwrap();
    assert_eq!(resolve(&fixture).err(), Some(ResolveError::Symlink));
}

#[test]
fn dashboard_path_occupied_by_a_regular_file_is_rejected_not_silently_treated_as_absent() {
    let fixture = fixture();
    let package = fixture.host.parent().unwrap().parent().unwrap();
    fs::write(package.join("dashboard"), b"not a directory").unwrap();
    assert_eq!(resolve(&fixture).err(), Some(ResolveError::Layout));
}

#[test]
fn dashboard_symlink_and_writable_permission_are_rejected() {
    let fixture = fixture();
    let package = fixture.host.parent().unwrap().parent().unwrap();
    let dashboard = package.join("dashboard");
    fs::create_dir_all(&dashboard).unwrap();
    fs::write(dashboard.join("index.html"), b"<html></html>").unwrap();
    replace_with_symlink(&dashboard);
    assert_eq!(resolve(&fixture).err(), Some(ResolveError::Symlink));
}

#[test]
fn dashboard_group_or_world_writable_is_rejected() {
    let fixture = fixture();
    let package = fixture.host.parent().unwrap().parent().unwrap();
    let dashboard = package.join("dashboard");
    fs::create_dir_all(&dashboard).unwrap();
    fs::write(dashboard.join("index.html"), b"<html></html>").unwrap();
    fs::set_permissions(&dashboard, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(resolve(&fixture).err(), Some(ResolveError::Ownership));
}

#[path = "../src/unix_socket.rs"]
#[allow(dead_code)]
mod unix_socket;

mod transport {
    use super::unix_socket::{HostError, invoking_uid, parse_arguments, session, verify_peer};
    use multicore_core::elevation_protocol::{
        ElevatedEngine, ElevationCommand, ElevationResponse, SessionSecret, decode_frame,
        read_stream_frame, write_stream_frame,
    };
    use std::{
        ffi::OsString,
        io::Write,
        os::unix::net::UnixStream,
        path::PathBuf,
        sync::{Arc, Mutex},
        thread,
    };

    const SOCKET: &str =
        "/run/user/1000/multicore/broker-0123456789abcdef0123456789abcdef0123456789abcdef.sock";

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments_accept_only_the_exact_generated_shape() {
        let parsed = parse_arguments(
            arguments(&["--socket", SOCKET, "--protocol", "1", "--server-pid", "42"]).into_iter(),
        )
        .unwrap();
        assert_eq!(parsed.socket, PathBuf::from(SOCKET));
        assert_eq!(parsed.protocol, 1);
        assert_eq!(parsed.server_pid, 42);

        for invalid in [
            vec!["--socket", SOCKET, "--protocol", "1"],
            vec!["--pipe", SOCKET, "--protocol", "1", "--server-pid", "42"],
            vec!["--socket", SOCKET, "--protocol", "1", "--server-pid", "0"],
            vec!["--socket", SOCKET, "--protocol", "x", "--server-pid", "42"],
            vec![
                "--socket",
                "relative/broker-0123456789abcdef0123456789abcdef0123456789abcdef.sock",
                "--protocol",
                "1",
                "--server-pid",
                "42",
            ],
            vec![
                "--socket",
                "/run/broker-0123.sock",
                "--protocol",
                "1",
                "--server-pid",
                "42",
            ],
            vec![
                "--socket",
                "/run/broker-0123456789ABCDEF0123456789abcdef0123456789abcdef.sock",
                "--protocol",
                "1",
                "--server-pid",
                "42",
            ],
            vec![
                "--socket",
                "/run/x.sock",
                "--protocol",
                "1",
                "--server-pid",
                "42",
                "--config",
                "/etc",
            ],
        ] {
            assert_eq!(
                parse_arguments(arguments(&invalid).into_iter()).err(),
                Some(HostError::InvalidArguments),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn invoking_uid_prefers_pkexec_and_rejects_garbage() {
        assert_eq!(invoking_uid(Some("1000")).unwrap(), 1000);
        assert_eq!(
            invoking_uid(Some("root")).err(),
            Some(HostError::InvalidArguments)
        );
        assert_eq!(
            invoking_uid(None).unwrap(),
            rustix::process::getuid().as_raw()
        );
    }

    #[test]
    fn peer_credentials_must_match_the_server_pid_and_invoking_uid() {
        let (host, _daemon) = UnixStream::pair().unwrap();
        let pid = std::process::id();
        let uid = rustix::process::getuid().as_raw();
        verify_peer(&host, pid, uid).unwrap();
        assert_eq!(
            verify_peer(&host, pid + 1, uid).err(),
            Some(HostError::PeerMismatch)
        );
        assert_eq!(
            verify_peer(&host, pid, uid + 1).err(),
            Some(HostError::PeerMismatch)
        );
    }

    #[test]
    fn session_echoes_authentication_then_serves_commands_until_shutdown() {
        let (host, mut daemon) = UnixStream::pair().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let server = thread::spawn(move || {
            session(host, move || {
                Ok(move |command: ElevationCommand| {
                    recorded.lock().unwrap().push(command.clone());
                    match command {
                        ElevationCommand::StartXray { .. } => ElevationResponse::Started {
                            engine: ElevatedEngine::Xray,
                        },
                        _ => ElevationResponse::ShuttingDown,
                    }
                })
            })
        });

        let secret = SessionSecret::generate().unwrap();
        write_stream_frame(&mut daemon, &secret.authentication()).unwrap();
        let echoed = decode_frame(&read_stream_frame(&mut daemon).unwrap()).unwrap();
        assert!(secret.verifies(&echoed));
        write_stream_frame(
            &mut daemon,
            &ElevationCommand::StartXray { generation_id: 7 },
        )
        .unwrap();
        let started: ElevationResponse =
            decode_frame(&read_stream_frame(&mut daemon).unwrap()).unwrap();
        assert_eq!(
            started,
            ElevationResponse::Started {
                engine: ElevatedEngine::Xray
            }
        );
        write_stream_frame(&mut daemon, &ElevationCommand::Shutdown).unwrap();
        let stopped: ElevationResponse =
            decode_frame(&read_stream_frame(&mut daemon).unwrap()).unwrap();
        assert_eq!(stopped, ElevationResponse::ShuttingDown);

        assert_eq!(server.join().unwrap(), Ok(()));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ElevationCommand::StartXray { generation_id: 7 },
                ElevationCommand::Shutdown
            ]
        );
    }

    #[test]
    fn session_stops_on_eof_or_invalid_frames_without_executing() {
        for payload in [
            None,
            Some(b"\x00\x00\x00\x00".to_vec()),
            Some(b"\x03\x00\x00\x00abc".to_vec()),
        ] {
            let (host, mut daemon) = UnixStream::pair().unwrap();
            let executed = Arc::new(Mutex::new(false));
            let flag = executed.clone();
            let server = thread::spawn(move || {
                session(host, move || {
                    *flag.lock().unwrap() = true;
                    Ok(|_: ElevationCommand| ElevationResponse::ShuttingDown)
                })
            });
            if let Some(payload) = &payload {
                daemon.write_all(payload).unwrap();
            }
            drop(daemon);
            let result = server.join().unwrap();
            assert!(result.is_err(), "{payload:?}");
            assert!(!*executed.lock().unwrap(), "{payload:?}");
        }
    }

    #[test]
    fn host_main_keeps_the_broker_failure_contract() {
        let main = include_str!("../src/main.rs");
        let transport = include_str!("../src/unix_socket.rs");
        for forbidden in [
            "--secret",
            "--token",
            "--config",
            "--url",
            "subscription_url",
        ] {
            assert!(!main.contains(forbidden));
            assert!(!transport.contains(forbidden));
        }
        for required in ["--socket", "--protocol", "--server-pid", "PKEXEC_UID"] {
            assert!(transport.contains(required));
        }
        assert!(main.contains("privileged broker stopped safely"));
    }
}

/// Review I4: the service reads staged configs through the handles opened (`O_NOFOLLOW`)
/// during resolution, so a symlink renamed over a config afterwards is never followed.
#[test]
fn guarded_config_read_ignores_a_symlink_swapped_in_after_resolution() {
    let fixture = fixture();
    let config = fixture.generation.join("mihomo.yaml");
    fs::write(&config, b"mixed-port: 1\n").unwrap();
    let resolved = resolve(&fixture).unwrap();
    let secret = fixture.data.join("secret.yaml");
    fs::write(&secret, b"secret: stolen\n").unwrap();
    let staged = fixture.generation.join("swap");
    symlink(&secret, &staged).unwrap();
    fs::rename(&staged, &config).unwrap();
    assert_eq!(
        resolved
            .read_guarded(&resolved.paths.mihomo_config, 1024)
            .unwrap(),
        b"mixed-port: 1\n"
    );
    assert!(resolved.verify_unchanged().is_err());
    assert_eq!(
        resolved
            .read_guarded(&resolved.paths.mihomo_config, 4)
            .err(),
        Some(ResolveError::Layout)
    );
}

/// A FIFO planted in place of a config must be rejected, never block the privileged opener.
#[test]
fn a_fifo_config_is_rejected_without_blocking() {
    let fixture = fixture();
    let config = fixture.generation.join("xray.json");
    fs::remove_file(&config).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&config)
            .status()
            .unwrap()
            .success()
    );
    assert!(resolve(&fixture).is_err());
}
