use std::{
    ffi::OsString,
    future::Future,
    io,
    os::unix::process::CommandExt,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, getpid, getppid, kill_process_group,
    set_parent_process_death_signal, waitid,
};

use crate::{
    CommandSpec, CoreLogBuffer, DiagnosticStream, Engine, ManagedChild, ProcessError,
    ProcessLauncher, RuntimeCheckState, RuntimePaths, TokioProcessLauncher,
    sidecar::{spawn_blocking_log_reader, wait_for_runtime_readiness},
};

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const EXIT_POLL: Duration = Duration::from_millis(20);
const ALLOWED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

type SpawnReply = tokio::sync::oneshot::Sender<io::Result<Child>>;

#[derive(Clone)]
pub struct UnixGroupLauncher {
    spawner: Arc<Mutex<mpsc::Sender<(CommandSpec, SpawnReply)>>>,
    logs: Arc<CoreLogBuffer>,
    readiness: Option<TokioProcessLauncher>,
    readiness_timeout: Duration,
}

impl UnixGroupLauncher {
    pub fn new(logs: Arc<CoreLogBuffer>) -> io::Result<Self> {
        let (sender, requests) = mpsc::channel::<(CommandSpec, SpawnReply)>();
        std::thread::Builder::new()
            .name("multicore-core-spawner".into())
            .spawn(move || {
                for (command, reply) in requests {
                    let _ = reply.send(spawn_core(&command));
                }
            })?;
        Ok(Self {
            spawner: Arc::new(Mutex::new(sender)),
            logs,
            readiness: None,
            readiness_timeout: crate::CORE_READINESS_TIMEOUT,
        })
    }

    pub fn for_runtime(
        paths: RuntimePaths,
        logs: Arc<CoreLogBuffer>,
        timeout: Duration,
    ) -> io::Result<Self> {
        let mut launcher = Self::new(logs.clone())?;
        launcher.readiness = Some(TokioProcessLauncher::for_runtime_with_logs(
            paths, timeout, logs,
        ));
        launcher.readiness_timeout = timeout;
        Ok(launcher)
    }

    async fn launch_core(
        &self,
        command: CommandSpec,
    ) -> Result<UnixGroupManagedChild, ProcessError> {
        let engine = command.engine;
        let (reply, response) = tokio::sync::oneshot::channel();
        self.spawner
            .lock()
            .map_err(|_| ProcessError::LaunchFailed(engine))?
            .send((command, reply))
            .map_err(|_| ProcessError::LaunchFailed(engine))?;
        let mut child = response
            .await
            .map_err(|_| ProcessError::LaunchFailed(engine))?
            .map_err(|_| ProcessError::LaunchFailed(engine))?;
        let group = Pid::from_child(&child);
        if let Some(stdout) = child.stdout.take() {
            spawn_blocking_log_reader(self.logs.clone(), engine, DiagnosticStream::Stdout, stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_blocking_log_reader(self.logs.clone(), engine, DiagnosticStream::Stderr, stderr);
        }
        Ok(UnixGroupManagedChild {
            engine,
            child,
            group,
            reaped: false,
        })
    }
}

fn sanitized_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    variables
        .into_iter()
        .filter(|(key, _)| {
            key.to_str()
                .is_some_and(|key| ALLOWED_ENVIRONMENT.contains(&key))
        })
        .collect()
}

fn spawn_core(command: &CommandSpec) -> io::Result<Child> {
    let parent = getpid();
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(sanitized_environment(std::env::vars_os()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    unsafe {
        process.pre_exec(move || {
            set_parent_process_death_signal(Some(Signal::KILL))?;
            if getppid() != Some(parent) {
                return Err(io::Error::other("core host exited before core start"));
            }
            Ok(())
        });
    }
    process.spawn()
}

pub struct UnixGroupManagedChild {
    engine: Engine,
    child: Child,
    group: Pid,
    reaped: bool,
}

impl UnixGroupManagedChild {
    fn exited_without_reaping(&self) -> Result<bool, ProcessError> {
        if self.reaped {
            return Ok(true);
        }
        waitid(
            WaitId::Pid(self.group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(|_| ProcessError::InspectFailed(self.engine))
    }

    fn kill_group_and_reap(&mut self) -> Result<(), ProcessError> {
        if self.reaped {
            return Ok(());
        }
        let _ = kill_process_group(self.group, Signal::KILL);
        self.child
            .wait()
            .map_err(|_| ProcessError::StopFailed(self.engine))?;
        self.reaped = true;
        Ok(())
    }
}

impl ManagedChild for UnixGroupManagedChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        self.exited_without_reaping()
    }

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.reaped {
                return Ok(());
            }
            let _ = kill_process_group(self.group, Signal::TERM);
            let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
            while !self.exited_without_reaping()? && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(EXIT_POLL).await;
            }
            self.kill_group_and_reap()
        })
    }
}

impl Drop for UnixGroupManagedChild {
    fn drop(&mut self) {
        let _ = self.kill_group_and_reap();
    }
}

impl ProcessLauncher for UnixGroupLauncher {
    type Child = UnixGroupManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(self.launch_core(command))
    }

    fn diagnostic_logs(&self) -> Vec<crate::CoreLogRecord> {
        self.logs.snapshot()
    }

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(wait_for_runtime_readiness(
            self.readiness.as_ref(),
            self.readiness_timeout,
            command,
            child,
        ))
    }

    fn current_ready<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(readiness) => readiness.current_ready(engine).await,
                None => true,
            }
        })
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(readiness) => readiness.tun_check().await,
                None => RuntimeCheckState::Unsupported,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        time::Instant,
    };

    fn shell(script: &str, extra: &Path) -> CommandSpec {
        CommandSpec {
            engine: Engine::Xray,
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("sh"),
                extra.as_os_str().to_owned(),
            ],
        }
    }

    fn process_is_gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    fn eventually(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        condition()
    }

    fn read_pid(path: &Path) -> i32 {
        assert!(eventually(Duration::from_secs(5), || {
            std::fs::read_to_string(path).is_ok_and(|text| text.ends_with('\n'))
        }));
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn stop_terminates_the_core_and_its_background_children() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let mut child = launcher
            .launch(shell(
                "sleep 60 & echo \"$!\" > \"$1\"; exec sleep 60",
                &pid_file,
            ))
            .await
            .unwrap();
        let grandchild = read_pid(&pid_file);
        let core = child.child.id() as i32;
        assert!(!child.has_exited().unwrap());

        child.stop().await.unwrap();
        assert!(child.has_exited().unwrap());
        assert!(process_is_gone(core));
        assert!(eventually(Duration::from_secs(3), || process_is_gone(
            grandchild
        )));
    }

    #[tokio::test]
    async fn stop_kills_a_core_that_ignores_sigterm_after_the_grace_period() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("trapped");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let mut child = launcher
            .launch(shell(
                "trap '' TERM; echo \"$$\" > \"$1\"; while :; do sleep 1; done",
                &marker,
            ))
            .await
            .unwrap();
        let core = child.child.id() as i32;
        assert_eq!(read_pid(&marker), core);
        let started = Instant::now();
        child.stop().await.unwrap();
        assert!(started.elapsed() >= TERMINATION_GRACE);
        assert!(started.elapsed() < TERMINATION_GRACE + Duration::from_secs(3));
        assert!(process_is_gone(core));
    }

    #[tokio::test]
    async fn dropping_the_launcher_kills_cores_through_the_parent_death_signal() {
        let temp = tempfile::tempdir().unwrap();
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let child = launcher
            .launch(shell("exec sleep 60", temp.path()))
            .await
            .unwrap();
        let core = child.child.id() as i32;
        drop(launcher);
        assert!(eventually(Duration::from_secs(3), || process_is_gone(core)));
        drop(child);
    }

    #[tokio::test]
    async fn dropping_a_child_kills_and_reaps_its_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let child = launcher
            .launch(shell(
                "sleep 60 & echo \"$!\" > \"$1\"; exec sleep 60",
                &pid_file,
            ))
            .await
            .unwrap();
        let grandchild = read_pid(&pid_file);
        let core = child.child.id() as i32;
        drop(child);
        assert!(!Path::new(&format!("/proc/{core}")).exists());
        assert!(eventually(Duration::from_secs(3), || process_is_gone(
            grandchild
        )));
    }

    #[tokio::test]
    async fn core_output_is_captured_in_the_bounded_log_buffer() {
        let temp = tempfile::tempdir().unwrap();
        let logs = Arc::new(CoreLogBuffer::default());
        let launcher = UnixGroupLauncher::new(logs.clone()).unwrap();
        let mut child = launcher
            .launch(shell("echo started; echo failed >&2", temp.path()))
            .await
            .unwrap();
        assert!(eventually(Duration::from_secs(5), || logs.snapshot().len() == 2));
        let records = logs.snapshot();
        assert!(records.iter().any(|record| {
            record.stream == DiagnosticStream::Stdout && record.message == "started"
        }));
        assert!(records.iter().any(|record| {
            record.stream == DiagnosticStream::Stderr && record.message == "failed"
        }));
        child.stop().await.unwrap();
    }

    #[test]
    fn environment_keeps_only_the_allowlist() {
        let kept = sanitized_environment([
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (
                OsString::from("MULTICORE_DAEMON_TOKEN"),
                OsString::from("secret"),
            ),
            (OsString::from("HOME"), OsString::from("/root")),
            (OsString::from("path"), OsString::from("/tmp")),
        ]);
        assert_eq!(
            kept,
            vec![
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (OsString::from("LANG"), OsString::from("C.UTF-8")),
            ]
        );
    }
}
