use std::{
    ffi::OsString,
    future::Future,
    net::TcpListener,
    path::PathBuf,
    pin::Pin,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use multicore_core::{
    CommandSpec, CoreLogBuffer, DiagnosticStream, Engine, ManagedChild, ProcessController,
    ProcessError, ProcessLauncher, RuntimeCheckState, RuntimePaths, SidecarProcessController,
    StartFailureCause, TokioProcessLauncher,
};

const SIDECAR_SOURCE: &str = include_str!("../src/sidecar.rs");

#[test]
fn tun_readiness_never_spawns_a_path_searched_shell() {
    assert!(!SIDECAR_SOURCE.contains("powershell.exe"));
    assert!(!SIDECAR_SOURCE.contains("Get-NetAdapter"));
    assert!(SIDECAR_SOURCE.contains("GetIfTable2"));
}

#[derive(Clone, Default)]
struct FakeLauncher {
    launches: Arc<Mutex<Vec<CommandSpec>>>,
    exited: Arc<AtomicBool>,
    stops: Arc<AtomicUsize>,
    fail_stop_once: Arc<AtomicBool>,
    fail_readiness: Arc<AtomicBool>,
}

struct FakeChild {
    exited: Arc<AtomicBool>,
    stops: Arc<AtomicUsize>,
    fail_stop_once: Arc<AtomicBool>,
}

impl ManagedChild for FakeChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        Ok(self.exited.load(Ordering::SeqCst))
    }

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            self.stops.fetch_add(1, Ordering::SeqCst);
            if self.fail_stop_once.swap(false, Ordering::SeqCst) {
                Err(ProcessError::StopFailed(Engine::Xray))
            } else {
                Ok(())
            }
        })
    }
}

impl ProcessLauncher for FakeLauncher {
    type Child = FakeChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let mut launches = self.launches.lock().unwrap();
            let restarting = !launches.is_empty();
            launches.push(command);
            drop(launches);
            if restarting {
                self.exited.store(false, Ordering::SeqCst);
            }
            Ok(FakeChild {
                exited: self.exited.clone(),
                stops: self.stops.clone(),
                fail_stop_once: self.fail_stop_once.clone(),
            })
        })
    }

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        _child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.fail_readiness.load(Ordering::SeqCst) {
                Err(ProcessError::ReadinessFailed(command.engine))
            } else {
                Ok(())
            }
        })
    }
}

fn paths() -> RuntimePaths {
    RuntimePaths {
        xray_binary: PathBuf::from(r"C:\Program Files\Multi Core\xray.exe"),
        mihomo_binary: PathBuf::from(r"C:\Program Files\Multi Core\mihomo.exe"),
        xray_config: PathBuf::from(r"C:\Users\Synthetic User\runtime\xray.json"),
        mihomo_config: PathBuf::from(r"C:\Users\Synthetic User\runtime\mihomo.yaml"),
        dashboard_directory: None,
        mihomo_home_directory: None,
    }
}

#[test]
fn runtime_paths_build_exact_sidecar_commands_without_shell_strings() {
    let paths = paths();
    let xray = paths.command(Engine::Xray);
    assert_eq!(xray.program, paths.xray_binary);
    assert_eq!(
        xray.args,
        [
            OsString::from("run"),
            OsString::from("-config"),
            paths.xray_config.clone().into_os_string(),
        ]
    );

    let mihomo = paths.command(Engine::Mihomo);
    assert_eq!(mihomo.program, paths.mihomo_binary);
    assert_eq!(
        mihomo.args,
        [
            OsString::from("-f"),
            paths.mihomo_config.clone().into_os_string(),
        ]
    );
    assert!(xray.env.is_empty());
    assert!(mihomo.env.is_empty(), "no dashboard configured yet");
}

#[test]
fn mihomo_command_carries_safe_paths_only_when_a_dashboard_is_configured() {
    let mut paths = paths();
    paths.dashboard_directory = Some(PathBuf::from(r"C:\Program Files\Multi Core\dashboard"));
    let mihomo = paths.command(Engine::Mihomo);
    assert_eq!(
        mihomo.env,
        [(
            OsString::from("SAFE_PATHS"),
            OsString::from(r"C:\Program Files\Multi Core\dashboard"),
        )]
    );
    assert!(
        paths.command(Engine::Xray).env.is_empty(),
        "Xray never needs Mihomo's external-ui allowlist"
    );
}

#[tokio::test]
async fn duplicate_start_and_stop_are_idempotent() {
    let launcher = FakeLauncher::default();
    let launches = launcher.launches.clone();
    let stops = launcher.stops.clone();
    let controller = SidecarProcessController::with_launcher(paths(), launcher);

    controller.start(Engine::Xray).await.unwrap();
    controller.start(Engine::Xray).await.unwrap();
    controller.start(Engine::Mihomo).await.unwrap();
    assert_eq!(launches.lock().unwrap().len(), 2);

    controller.stop(Engine::Xray).await.unwrap();
    controller.stop(Engine::Xray).await.unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exited_child_can_be_started_again() {
    let launcher = FakeLauncher::default();
    let launches = launcher.launches.clone();
    let exited = launcher.exited.clone();
    let controller = SidecarProcessController::with_launcher(paths(), launcher);

    controller.start(Engine::Xray).await.unwrap();
    exited.store(true, Ordering::SeqCst);
    controller.start(Engine::Xray).await.unwrap();
    assert_eq!(launches.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_child_that_exits_during_startup_is_not_reported_ready() {
    let launcher = FakeLauncher::default();
    launcher.exited.store(true, Ordering::SeqCst);
    let controller = SidecarProcessController::with_launcher(paths(), launcher);

    assert!(controller.start(Engine::Xray).await.is_err());
}

#[test]
fn sidecar_start_failures_distinguish_readiness_timeout_from_core_failure() {
    let controller = SidecarProcessController::with_launcher(paths(), FakeLauncher::default());
    assert_eq!(
        controller.start_failure_cause(&ProcessError::ReadinessTimedOut(Engine::Mihomo)),
        StartFailureCause::ReadinessTimedOut
    );
    for error in [
        ProcessError::ReadinessFailed(Engine::Xray),
        ProcessError::LaunchFailed(Engine::Xray),
    ] {
        assert_eq!(
            controller.start_failure_cause(&error),
            StartFailureCause::CoreFailed
        );
    }
}

#[tokio::test]
async fn readiness_failure_stops_the_new_child_and_is_not_reported_ready() {
    let launcher = FakeLauncher::default();
    launcher.fail_readiness.store(true, Ordering::SeqCst);
    let stops = launcher.stops.clone();
    let controller = SidecarProcessController::with_launcher(paths(), launcher);

    assert_eq!(
        controller.start(Engine::Xray).await,
        Err(ProcessError::ReadinessFailed(Engine::Xray))
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn diagnostics_distinguish_ready_and_stopped_owned_cores() {
    let controller = SidecarProcessController::with_launcher(paths(), FakeLauncher::default());
    controller.start(Engine::Xray).await.unwrap();

    let diagnostics = controller.diagnostics().await;
    assert_eq!(diagnostics.xray, RuntimeCheckState::Ready);
    assert_eq!(diagnostics.mihomo, RuntimeCheckState::Stopped);
    assert_eq!(diagnostics.tun, RuntimeCheckState::Stopped);
    assert!(diagnostics.logs.is_empty());
}

#[test]
fn diagnostic_log_buffer_redacts_truncates_and_evicts_old_lines() {
    let logs = CoreLogBuffer::with_limits(2, 24);
    logs.push(
        Engine::Xray,
        DiagnosticStream::Stderr,
        "Authorization: Bearer private-secret",
    );
    logs.push(
        Engine::Mihomo,
        DiagnosticStream::Stdout,
        "https://example.invalid/private/path",
    );
    logs.push(
        Engine::Mihomo,
        DiagnosticStream::Stderr,
        "abcdefghijklmnopqrstuvwxyz0123456789",
    );

    let records = logs.snapshot();
    assert_eq!(records.len(), 2);
    assert!(records[0].message.contains("[redacted]"));
    assert!(!records[0].message.contains("example.invalid"));
    assert!(records[1].message.chars().count() <= 24);
    assert!(!format!("{records:?}").contains("private-secret"));
}

#[test]
fn persistent_diagnostic_log_is_redacted_and_survives_buffer_drop() {
    let root = std::env::temp_dir().join(format!(
        "multicore-persistent-log-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("worker")
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).unwrap();
    let path = root.join("latest-core.log");
    std::fs::write(&path, "stale session must be replaced").unwrap();

    {
        let logs = CoreLogBuffer::persistent(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        logs.push(
            Engine::Xray,
            DiagnosticStream::Stderr,
            "Authorization: Bearer private-secret",
        );
        logs.push(
            Engine::Mihomo,
            DiagnosticStream::Stdout,
            "dial https://example.invalid/private/path\nnext line",
        );
    }

    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("xray\tstderr\tAuthorization: [redacted]"));
    assert!(persisted.contains("mihomo\tstdout\tdial [redacted] next line"));
    assert!(!persisted.contains("private-secret"));
    assert!(!persisted.contains("example.invalid"));
    assert!(!persisted.contains("stale session"));
    assert_eq!(persisted.lines().count(), 2);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn persistent_log_failure_falls_back_to_the_memory_dashboard() {
    let root = std::env::temp_dir().join(format!("multicore-log-fallback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).unwrap();

    let logs = CoreLogBuffer::persistent_or_memory(&root);
    logs.push(Engine::Mihomo, DiagnosticStream::Stderr, "fallback marker");
    assert_eq!(logs.snapshot()[0].message, "fallback marker");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn tokio_launcher_captures_and_redacts_both_output_streams() {
    let launcher = TokioProcessLauncher::default();
    let command = CommandSpec {
        engine: Engine::Xray,
        program: std::env::current_exe().unwrap(),
        args: [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("core_output_observer_helper"),
            OsString::from("--nocapture"),
        ]
        .into(),
        env: Vec::new(),
    };
    let mut child = launcher.launch(command).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !child.has_exited().unwrap() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;

    let records = launcher.logs().snapshot();
    let rendered = format!("{records:?}");
    assert!(rendered.contains("stdout marker"));
    assert!(rendered.contains("stderr marker"));
    assert!(rendered.contains("[redacted]"));
    assert!(!rendered.contains("example.invalid"));
    assert!(!rendered.contains("private-secret"));
}

#[test]
#[ignore = "isolated helper invoked by tokio_launcher_captures_and_redacts_both_output_streams"]
fn core_output_observer_helper() {
    println!("stdout marker https://example.invalid/private");
    eprintln!("stderr marker Authorization: Bearer private-secret");
}

#[tokio::test]
async fn runtime_readiness_requires_declared_xray_loopback_ports() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = std::env::temp_dir().join(format!(
        "multicore-sidecar-readiness-{}-{port}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let xray_config = root.join("xray.json");
    std::fs::write(
        &xray_config,
        format!(r#"{{"inbounds":[{{"listen":"127.0.0.1","port":{port}}}]}}"#),
    )
    .unwrap();
    let runtime_paths = RuntimePaths {
        xray_binary: PathBuf::new(),
        mihomo_binary: PathBuf::new(),
        xray_config,
        mihomo_config: root.join("mihomo.yaml"),
        dashboard_directory: None,
        mihomo_home_directory: None,
    };
    let launcher = TokioProcessLauncher::for_runtime(runtime_paths, Duration::from_millis(200));
    let command = CommandSpec {
        engine: Engine::Xray,
        program: std::env::current_exe().unwrap(),
        args: [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("readiness_process_helper"),
            OsString::from("--nocapture"),
        ]
        .into(),
        env: Vec::new(),
    };
    let mut child = launcher.launch(command.clone()).await.unwrap();
    assert!(
        launcher
            .wait_until_ready(&command, &mut child)
            .await
            .is_ok()
    );
    child.stop().await.unwrap();

    drop(listener);
    let mut child = launcher.launch(command.clone()).await.unwrap();
    assert_eq!(
        launcher.wait_until_ready(&command, &mut child).await,
        Err(ProcessError::ReadinessTimedOut(Engine::Xray))
    );
    child.stop().await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore = "isolated helper invoked by runtime_readiness_requires_declared_xray_loopback_ports"]
fn readiness_process_helper() {
    std::thread::sleep(Duration::from_secs(5));
}

/// The readiness poll loop's sleep interval directly bounds how much pure waste a Connect can
/// incur when a core becomes ready just after a check. It must stay small (a real regression to
/// a coarse ~100ms tick would waste up to 100ms per check) without busy-looping.
///
/// This asserts on source text rather than wall-clock timing: real TCP connect latency to a
/// not-yet-listening loopback port is not reliable to bound tightly across every environment
/// (sandboxed/monitored networking stacks can hold a pending SYN far longer than a bare OS
/// would), so a wall-clock assertion here would be flaky. The interval itself is a plain
/// constant, so asserting its value directly is both exact and deterministic.
#[test]
fn readiness_poll_interval_is_tight_but_not_a_busy_loop() {
    assert!(
        !SIDECAR_SOURCE.contains("sleep(Duration::from_millis(100))"),
        "a readiness loop reverted to the old, wasteful 100ms poll tick"
    );
    let occurrences = SIDECAR_SOURCE.matches("READINESS_POLL_INTERVAL").count();
    // One definition plus both readiness loops (`wait_until_ready` and
    // `wait_for_runtime_readiness`) actually using it.
    assert!(
        occurrences >= 3,
        "expected both readiness poll loops to share one tight interval constant"
    );
}

#[tokio::test]
async fn failed_stop_retains_child_handle_for_retry() {
    let launcher = FakeLauncher::default();
    let stops = launcher.stops.clone();
    launcher.fail_stop_once.store(true, Ordering::SeqCst);
    let controller = SidecarProcessController::with_launcher(paths(), launcher);

    controller.start(Engine::Xray).await.unwrap();
    assert_eq!(
        controller.stop(Engine::Xray).await,
        Err(ProcessError::StopFailed(Engine::Xray))
    );
    controller.stop(Engine::Xray).await.unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[test]
fn real_core_child_cannot_inherit_bootstrap_environment() {
    let output =
        std::env::temp_dir().join(format!("multicore-sidecar-env-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&output);
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "real_core_child_environment_launcher_helper",
            "--nocapture",
        ])
        .env("MULTICORE_SIDECAR_TEST_OUTPUT", &output)
        .env("MULTICORE_DAEMON_URL", "http://127.0.0.1:8787")
        .env("MULTICORE_DAEMON_TOKEN", "secret-marker")
        .env("MULTICORE_DAEMON_READY_STDOUT", "1")
        .env("MULTICORE_DAEMON_ADDR", "127.0.0.1:1234")
        .env("MULTICORE_DAEMON_DATA_DIR", "private-path")
        .env("MULTICORE_XRAY_BIN", "xray-private-path")
        .env("MULTICORE_MIHOMO_BIN", "mihomo-private-path")
        .env("MULTICORE_MIHOMO_CONTROLLER_ADDR", "192.0.2.1:19090")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match helper.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = helper.kill();
                let _ = helper.wait();
                panic!("isolated launcher helper timed out");
            }
            Err(error) => {
                let _ = helper.kill();
                let _ = helper.wait();
                panic!("failed to inspect isolated launcher helper: {error}");
            }
        }
    };
    let _ = helper.wait();
    assert!(status.success(), "isolated launcher helper failed");
    let inherited = std::fs::read_to_string(&output).unwrap();
    let _ = std::fs::remove_file(output);
    assert_eq!(inherited, "false");
}

#[tokio::test]
#[ignore = "isolated helper invoked by real_core_child_cannot_inherit_bootstrap_environment"]
async fn real_core_child_environment_launcher_helper() {
    let launch = TokioProcessLauncher::default()
        .launch(CommandSpec {
            engine: Engine::Xray,
            program: std::env::current_exe().unwrap(),
            args: [
                OsString::from("--ignored"),
                OsString::from("--exact"),
                OsString::from("real_core_child_environment_observer_helper"),
                OsString::from("--nocapture"),
            ]
            .into(),
            env: Vec::new(),
        })
        .await;
    let mut child = match launch {
        Ok(child) => child,
        Err(error) => panic!("launch environment observer: {error}"),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let failure = loop {
        match child.has_exited() {
            Ok(true) => break None,
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(false) => break Some("environment observer timed out"),
            Err(_) => break Some("failed to inspect environment observer"),
        }
    };
    if let Some(message) = failure {
        let cleanup = child.stop().await;
        assert!(cleanup.is_ok(), "{message}; cleanup failed: {cleanup:?}");
        panic!("{message}");
    }
}

#[test]
#[ignore = "isolated helper invoked by real_core_child_environment_launcher_helper"]
fn real_core_child_environment_observer_helper() {
    let inherited = [
        "MULTICORE_DAEMON_URL",
        "MULTICORE_DAEMON_TOKEN",
        "MULTICORE_DAEMON_READY_STDOUT",
        "MULTICORE_DAEMON_ADDR",
        "MULTICORE_DAEMON_DATA_DIR",
        "MULTICORE_XRAY_BIN",
        "MULTICORE_MIHOMO_BIN",
        "MULTICORE_MIHOMO_CONTROLLER_ADDR",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some());
    let output = std::env::var_os("MULTICORE_SIDECAR_TEST_OUTPUT").unwrap();
    std::fs::write(output, inherited.to_string()).unwrap();
}

#[test]
fn service_mihomo_runs_in_its_own_home_directory() {
    let mut paths = paths();
    let home = PathBuf::from(r"C:\ProgramData\MultiCore\service");
    paths.mihomo_home_directory = Some(home.clone());
    let spec = paths.command(Engine::Mihomo);
    assert_eq!(
        spec.args,
        vec![
            OsString::from("-d"),
            home.clone().into_os_string(),
            OsString::from("-f"),
            paths.mihomo_config.clone().into_os_string(),
        ]
    );
    assert_eq!(
        spec.env,
        vec![(OsString::from("SAFE_PATHS"), home.into_os_string())]
    );
    let xray = paths.command(Engine::Xray);
    assert!(xray.env.is_empty());
}

#[tokio::test]
async fn the_multicore_tun_probe_answers_promptly_and_consistently() {
    // The dev PC may run the owner's own MultiCore tunnel, so the answer itself is machine
    // state; the alias/up rule is pinned by `adapter_requires_exact_alias_and_operational_up`.
    let first = tokio::time::timeout(
        Duration::from_secs(5),
        multicore_core::multicore_tun_is_up(),
    )
    .await
    .expect("the probe never blocks");
    let second = multicore_core::multicore_tun_is_up().await;
    assert_eq!(first, second);
}

/// A service TUN generation starts Mihomo with TUN off and no inbound port at all (plan
/// amendment A6), so its readiness is the controller alone. Outside service mode the same
/// config stays "not runnable", as before.
#[tokio::test]
async fn service_mihomo_without_ports_is_ready_once_its_controller_listens() {
    let controller = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = controller.local_addr().unwrap().port();
    let root = tempfile::tempdir().unwrap();
    let mihomo_config = root.path().join("mihomo.yaml");
    std::fs::write(
        &mihomo_config,
        format!(
            "external-controller: 127.0.0.1:{port}\nbind-address: 127.0.0.1\ntun:\n  enable: false\n  device: MultiCore\n"
        ),
    )
    .unwrap();
    let on_demand = RuntimePaths {
        xray_binary: PathBuf::new(),
        mihomo_binary: PathBuf::new(),
        xray_config: root.path().join("xray.json"),
        mihomo_config,
        dashboard_directory: None,
        mihomo_home_directory: None,
    };
    let service = RuntimePaths {
        mihomo_home_directory: Some(root.path().to_path_buf()),
        ..on_demand.clone()
    };
    let timeout = Duration::from_millis(200);
    assert!(
        !TokioProcessLauncher::for_runtime(on_demand, timeout)
            .current_ready(Engine::Mihomo)
            .await
    );
    let service = TokioProcessLauncher::for_runtime(service, timeout);
    assert!(service.current_ready(Engine::Mihomo).await);
    assert_eq!(service.tun_check().await, RuntimeCheckState::Disabled);
    drop(controller);
    assert!(!service.current_ready(Engine::Mihomo).await);
}

/// The persistent core log rotates at a fixed size and keeps exactly one predecessor, so a
/// chatty core can never fill the disk (a long-running service writes it for days).
#[test]
fn persistent_log_rotates_and_keeps_at_most_two_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("latest-core.log");
    let line = "x".repeat(2_000);
    let logs = CoreLogBuffer::persistent(&path).unwrap();
    let lines = (2 * multicore_core::MAX_PERSISTENT_LOG_BYTES / 2_000 + 500) as usize;
    for _ in 0..lines {
        logs.push(Engine::Mihomo, DiagnosticStream::Stdout, &line);
    }
    let rotated = multicore_core::rotated_log_path(&path);
    assert_eq!(rotated, root.path().join("latest-core.log.1"));
    let current = std::fs::metadata(&path).unwrap().len();
    let previous = std::fs::metadata(&rotated).unwrap().len();
    assert!(current > 0 && current <= multicore_core::MAX_PERSISTENT_LOG_BYTES);
    assert!(previous > multicore_core::MAX_PERSISTENT_LOG_BYTES / 2);
    assert!(previous <= multicore_core::MAX_PERSISTENT_LOG_BYTES);
    let files = std::fs::read_dir(root.path()).unwrap().count();
    assert_eq!(files, 2, "only the log and one rotated predecessor");
}
