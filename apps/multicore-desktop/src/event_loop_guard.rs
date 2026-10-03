//! The event loop ends only when the app asks it to.
//!
//! Slint's winit backend leaves its event loop on the first error a window returns while
//! drawing, resizing or handling an event, and `slint::run_event_loop_until_quit` hands that
//! error back. For a Skia Direct3D window that is any failed `Present`, fence or swap-chain
//! call («Error presenting d3d swap chain: 887a0005» once the GPU driver removed the device).
//! 1.1.6 and 1.1.7 then left `run_desktop` exactly as on «Выйти»: the owned daemon was asked
//! to disconnect, the window vanished, and the error went to a stderr that a Windows GUI app
//! does not have. Reports showed only «Подключение остановлено.» at the moment the window
//! closed, no crash.log and no Windows error event.
//!
//! [`run_until_quit`] keeps running the loop until [`request_quit`] was called. Any other exit
//! is written to `logs/crash.log` (and a line to `client-errors.log`); then every window gets
//! a new native window and renderer (a new Direct3D device) and the loop runs again. Only
//! when that keeps failing ([`MAX_RECOVERIES`] within [`RECOVERY_WINDOW`]) does the error
//! reach `main`, which restarts with the software renderer when it names the GPU renderer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor as _;

/// Recoveries allowed within [`RECOVERY_WINDOW`] before the error is final.
pub(crate) const MAX_RECOVERIES: usize = 3;
pub(crate) const RECOVERY_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Set by [`request_quit`]: the app itself ends the event loop.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ends the event loop for good («Выйти», an update or service install taking over, a data
/// reset). Every exit the app means goes through here; `slint::quit_event_loop` alone would
/// be taken for a failure and the loop restarted.
pub(crate) fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
    let _ = slint::quit_event_loop();
}

fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// Remembers recent recoveries; refuses once [`MAX_RECOVERIES`] happened within
/// [`RECOVERY_WINDOW`].
#[derive(Debug, Default)]
pub(crate) struct RecoveryBudget {
    recent: VecDeque<Instant>,
}

impl RecoveryBudget {
    /// Takes one recovery at `now`; `None` when the budget is spent, else the attempt number
    /// within the window (1-based).
    pub(crate) fn take(&mut self, now: Instant) -> Option<usize> {
        while self
            .recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= RECOVERY_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_RECOVERIES {
            return None;
        }
        self.recent.push_back(now);
        Some(self.recent.len())
    }
}

/// What ended the loop, in words for the crash log.
pub(crate) fn describe_exit(result: &Result<(), slint::PlatformError>) -> String {
    match result {
        Ok(()) => "the event loop stopped without an error and without a quit request".into(),
        Err(error) => format!("the event loop stopped with an error: {error}"),
    }
}

/// What the supervisor does after the loop returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The app asked for it: return the loop's result.
    Return,
    /// Unexpected: recreate the windows and run the loop again (attempt n).
    Recover(usize),
    /// Unexpected and the recovery budget is spent: return the error.
    GiveUp,
}

pub(crate) fn decide(quit_requested: bool, budget: &mut RecoveryBudget, now: Instant) -> Next {
    if quit_requested {
        return Next::Return;
    }
    budget.take(now).map_or(Next::GiveUp, Next::Recover)
}

/// Runs the event loop until [`request_quit`]. After any other exit `recreate_windows` gives
/// every window a new native window and renderer before the loop runs again.
pub(crate) fn run_until_quit(
    mut recreate_windows: impl FnMut(),
) -> Result<(), slint::PlatformError> {
    let mut budget = RecoveryBudget::default();
    loop {
        let result = slint::run_event_loop_until_quit();
        let next = decide(quit_requested(), &mut budget, Instant::now());
        if next == Next::Return {
            return result;
        }
        let detail = describe_exit(&result);
        match next {
            Next::Recover(attempt) => {
                crate::crash_log::record_event_loop_exit(
                    &detail,
                    &format!(
                        "recreating the windows and their renderer, attempt {attempt} of {MAX_RECOVERIES} within {} min",
                        RECOVERY_WINDOW.as_secs() / 60
                    ),
                );
                // A GPU driver resetting after a removed device takes a moment; device creation
                // fails until it is back.
                std::thread::sleep(Duration::from_millis(250 * attempt as u64));
                recreate_windows();
            }
            _ => {
                crate::crash_log::record_event_loop_exit(
                    &detail,
                    "giving up: the renderer kept failing; the app exits",
                );
                return Err(result.err().unwrap_or_else(|| {
                    slint::PlatformError::Other(
                        "the event loop kept stopping without a quit request".into(),
                    )
                }));
            }
        }
    }
}

/// Environment variable that makes Slint 1.17 destroy a window (native window, renderer and
/// its GPU device) on `hide()` instead of only hiding it.
const DESTROY_ON_HIDE: &str = "SLINT_DESTROY_WINDOW_ON_HIDE";

/// Destroys `window`'s native window and renderer through Slint, which creates both anew on
/// the next `show()` with the same size and maximized state. The window is hidden afterwards.
pub(crate) fn destroy_native_window(window: &slint::Window) -> Result<(), slint::PlatformError> {
    let size = window.size().to_logical(window.scale_factor());
    let maximized = window.is_maximized();
    let preset = std::env::var_os(DESTROY_ON_HIDE).is_some();
    if !preset {
        // SAFETY: the standard library documents `set_var` and `remove_var` as always safe
        // on Windows, also with other threads running. On Unix this runs on the UI thread
        // between event loop runs, and no other thread of the app reads the environment.
        unsafe { std::env::set_var(DESTROY_ON_HIDE, "1") };
    }
    let hidden = window.hide();
    if !preset {
        // SAFETY: as above.
        unsafe { std::env::remove_var(DESTROY_ON_HIDE) };
    }
    hidden?;
    if !window.has_winit_window() {
        // Slint keeps the last size and position for the new window but not a maximized
        // state it already believes in; refresh it like the Wayland path does.
        window.set_size(size);
        if maximized {
            window.set_maximized(false);
            window.set_maximized(true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requested_quit_ends_the_loop_and_spends_nothing() {
        let mut budget = RecoveryBudget::default();
        let now = Instant::now();
        assert_eq!(decide(true, &mut budget, now), Next::Return);
        assert_eq!(budget.recent.len(), 0);
    }

    #[test]
    fn an_unexpected_exit_recovers_a_few_times_then_gives_up() {
        let mut budget = RecoveryBudget::default();
        let start = Instant::now();
        for attempt in 1..=MAX_RECOVERIES {
            assert_eq!(
                decide(
                    false,
                    &mut budget,
                    start + Duration::from_secs(attempt as u64)
                ),
                Next::Recover(attempt)
            );
        }
        assert_eq!(
            decide(false, &mut budget, start + Duration::from_secs(60)),
            Next::GiveUp
        );
        // Old recoveries age out: once the first two are older than the window, a failure
        // is recovered again.
        assert_eq!(
            decide(
                false,
                &mut budget,
                start + RECOVERY_WINDOW + Duration::from_secs(2)
            ),
            Next::Recover(2)
        );
    }

    #[test]
    fn the_exit_is_described_with_the_renderer_error() {
        let error: Result<(), slint::PlatformError> = Err(slint::PlatformError::Other(
            "Error presenting d3d swap chain: 887a0005".into(),
        ));
        assert_eq!(
            describe_exit(&error),
            "the event loop stopped with an error: Error presenting d3d swap chain: 887a0005"
        );
        assert!(describe_exit(&Ok(())).contains("without a quit request"));
    }

    /// What the scripted platform's event loop does on each run.
    enum Run {
        /// Returns this error, as Slint's winit backend does after a failed `Present`.
        Fail(&'static str),
        /// The app quits («Выйти»).
        Quit,
    }

    thread_local! {
        static SCRIPT: std::cell::RefCell<VecDeque<Run>> = const {
            std::cell::RefCell::new(VecDeque::new())
        };
        static RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    struct ScriptedPlatform;

    impl slint::platform::Platform for ScriptedPlatform {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Err(slint::PlatformError::NoPlatform)
        }

        fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
            RUNS.set(RUNS.get() + 1);
            match SCRIPT.with(|script| script.borrow_mut().pop_front()) {
                Some(Run::Fail(error)) => Err(slint::PlatformError::Other(error.into())),
                Some(Run::Quit) => {
                    request_quit();
                    Ok(())
                }
                None => Err(slint::PlatformError::Other("script exhausted".into())),
            }
        }
    }

    /// 1.1.6/1.1.7 closed outright when the event loop returned a renderer error. Now the
    /// windows are recreated and the loop runs again; only a renderer that keeps failing ends
    /// the app, with an error that sends `main` to the software renderer.
    #[test]
    fn a_renderer_error_recreates_the_windows_and_the_loop_runs_on() {
        slint::platform::set_platform(Box::new(ScriptedPlatform)).expect("scripted platform");
        let _log = crate::crash_log::TEST_LOG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let crash_log = crate::crash_log::crash_log_path().expect("test crash log");

        // Failing every time: recovered MAX_RECOVERIES times, then the error is final.
        SCRIPT.with(|script| {
            script.borrow_mut().extend(
                (0..=MAX_RECOVERIES)
                    .map(|_| Run::Fail("Error presenting d3d swap chain: 887a0005")),
            )
        });
        let mut recreated = 0;
        let error = run_until_quit(|| recreated += 1).expect_err("a renderer that keeps failing");
        assert_eq!(recreated, MAX_RECOVERIES);
        assert_eq!(RUNS.get(), MAX_RECOVERIES + 1);
        assert!(
            crate::renderer_fallback::is_gpu_renderer_failure(&error),
            "{error}"
        );
        let log = std::fs::read_to_string(&crash_log).expect("crash log");
        assert!(
            log.contains(
                "Error presenting d3d swap chain: 887a0005
recreating the windows"
            ),
            "{log}"
        );
        assert!(log.contains("giving up"), "{log}");

        // One failure, then the user quits: the app stays up in between and ends normally.
        RUNS.set(0);
        SCRIPT.with(|script| {
            script.borrow_mut().extend([
                Run::Fail(
                    "error setting up completion signal for d3d12 command queue failed. 887a0005",
                ),
                Run::Quit,
            ])
        });
        let mut recreated = 0;
        run_until_quit(|| recreated += 1).expect("quit after the recovery");
        assert_eq!((recreated, RUNS.get()), (1, 2));
    }

    /// Every exit the app means goes through `request_quit`; a bare `slint::quit_event_loop`
    /// would be taken for a renderer failure and the loop restarted.
    #[test]
    fn the_app_quits_only_through_request_quit() {
        let main = include_str!("main.rs");
        assert!(!main.contains(concat!("slint::", "quit_event_loop()")));
        assert!(main.contains("event_loop_guard::request_quit()"));
        assert!(!main.contains(concat!("slint::", "run_event_loop_until_quit()")));
        assert!(main.contains("event_loop_guard::run_until_quit("));
    }
}
