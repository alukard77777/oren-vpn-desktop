#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_icon;
mod bootstrap;
mod clipboard_text;
mod daemon;
mod logo_cache;
mod preferences;
mod single_instance;
mod tray;
mod updater;
mod view_model;
mod windows_settings;
mod windows_shell;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use daemon::{DaemonClient, DaemonError, UnavailableDaemonClient};
use preferences::{AppPreferences, PreferenceStore, VisiblePage, WindowBounds};
use slint::winit_030::WinitWindowAccessor;
use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use tray::{
    TrayAccordionState, TrayCommand, TrayGroup, TrayInput, TrayInteraction, TrayMenuModel,
    TrayNode, TrayPopupModelCache, build_menu_model,
};
use updater::{UpdateAction, UpdateState};
use view_model::{
    CatalogDisplayKind, CatalogPresentation, DesktopViewModel, DiagnosticsPresentation,
    LatencyPresentation, LatencyRequest, MutationToken, NodeSelectionRequest, Panel, PrimaryAction,
    ProfileRequestToken, QueuedNodeSelection, SemanticTone, StatePresentation,
    SubscriptionPresentation, SubscriptionProfileOperation, SubscriptionProfilesPresentation,
    derive_catalog_display,
};
use windows_settings::LaunchAtSignInState;
use windows_shell::{
    CloseDisposition, MinimizeDisposition, MonitorRect, MonitorSignature, PhysicalPosition,
    apply_tray_popup_native_rounding, close_disposition, minimize_disposition, monitor_cap_changed,
    next_maximized, parse_resize_edge, place_tray_popup, restore_placement,
};

slint::include_modules!();

#[derive(Clone)]
struct UiSnapshot {
    presentation: StatePresentation,
    panel: Panel,
    import_pending: bool,
    import_error: String,
    subscription: SubscriptionPresentation,
    profiles: SubscriptionProfilesPresentation,
    catalog: CatalogPresentation,
    diagnostics: DiagnosticsPresentation,
    latency: LatencyPresentation,
}

struct ProfilePreferenceReconciler {
    hints: AppPreferences,
    current_profile: Option<String>,
    pending_ready_profile: Option<String>,
    tracker: Arc<Mutex<PersistenceTracker>>,
}

impl ProfilePreferenceReconciler {
    fn new(loaded: AppPreferences, tracker: Arc<Mutex<PersistenceTracker>>) -> Self {
        Self {
            hints: loaded,
            current_profile: None,
            pending_ready_profile: None,
            tracker,
        }
    }

    fn capture_current(&mut self, model: &DesktopViewModel) {
        let Some((profile, selected_group, selections)) = model.route_preferences() else {
            return;
        };
        self.record_hint(profile, selected_group, selections);
    }

    fn record_hint(
        &mut self,
        profile: String,
        selected_group: Option<String>,
        selections: BTreeMap<String, String>,
    ) {
        if let Some(group) = &selected_group {
            self.hints
                .last_group_by_profile
                .insert(profile.clone(), group.clone());
        } else {
            self.hints.last_group_by_profile.remove(&profile);
        }
        self.hints
            .selections_by_profile
            .insert(profile.clone(), selections.clone());
        self.tracker
            .lock()
            .expect("preference tracker lock")
            .set_route_preferences(profile, selected_group, selections);
    }

    fn reconcile_profile_list(&mut self, model: &DesktopViewModel) {
        self.set_authoritative_profile(model.profile_presentation().active_id);
    }

    fn set_authoritative_profile(&mut self, active_id: Option<String>) {
        self.tracker
            .lock()
            .expect("preference tracker lock")
            .set_active_profile_hint(active_id.clone());
        if self.current_profile == active_id {
            return;
        }
        self.current_profile.clone_from(&active_id);
        self.pending_ready_profile = active_id;
    }

    fn apply_ready_hints_after_catalog(
        &mut self,
        model: &mut DesktopViewModel,
        catalog_succeeded: bool,
    ) {
        if !catalog_succeeded {
            return;
        }
        let presentation = model.presentation();
        if presentation.is_connected {
            self.pending_ready_profile = None;
            return;
        }
        if !route_hints_apply_to(&presentation) {
            return;
        }
        let Some(profile) = self.pending_ready_profile.take() else {
            return;
        };
        let selected_group = self.hints.last_group_by_profile.get(&profile).cloned();
        let selections = self
            .hints
            .selections_by_profile
            .get(&profile)
            .cloned()
            .unwrap_or_default();
        let catalog_group = model.catalog_presentation().selected_group_id;
        model.restore_route_preferences(selected_group.as_deref(), &BTreeMap::new());
        for (group_id, node_id) in &selections {
            if model.select_catalog_group(group_id) {
                let _ = model.begin_node_selection(node_id);
            }
        }
        let restored_group = selected_group
            .as_deref()
            .is_some_and(|group_id| model.select_catalog_group(group_id));
        if !restored_group && let Some(group_id) = catalog_group {
            let _ = model.select_catalog_group(&group_id);
        }
    }
}

fn route_hints_apply_to(presentation: &StatePresentation) -> bool {
    presentation.primary_action == PrimaryAction::Connect
        && presentation.primary_enabled
        && !presentation.is_connected
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowObservation {
    restored_bounds: Option<WindowBounds>,
    maximized: bool,
    visible_page: VisiblePage,
}

struct PersistenceTracker {
    persisted: AppPreferences,
    desired: AppPreferences,
    candidate: Option<WindowObservation>,
    queued: Option<AppPreferences>,
}

impl PersistenceTracker {
    fn new(persisted: AppPreferences) -> Self {
        Self {
            desired: persisted.clone(),
            persisted,
            candidate: None,
            queued: None,
        }
    }

    fn incorporate_observation(&mut self, observation: &WindowObservation) {
        let mut next = self.desired.clone();
        if let Some(bounds) = &observation.restored_bounds {
            next.restored_bounds = Some(bounds.clone());
        }
        next.maximized = observation.maximized;
        next.visible_page = observation.visible_page.clone();
        self.desired = next;
    }

    fn set_ambient_background(&mut self, enabled: bool) {
        self.desired.ambient_background = enabled;
    }

    fn set_route_preferences(
        &mut self,
        profile: String,
        selected_group: Option<String>,
        selections: BTreeMap<String, String>,
    ) {
        if let Some(group) = selected_group {
            self.desired
                .last_group_by_profile
                .insert(profile.clone(), group);
        } else {
            self.desired.last_group_by_profile.remove(&profile);
        }
        self.desired
            .selections_by_profile
            .insert(profile, selections);
    }

    fn set_active_profile_hint(&mut self, profile: Option<String>) {
        self.desired.active_profile_hint = profile;
    }

    fn observe(&mut self, observation: Option<WindowObservation>) -> Option<AppPreferences> {
        let Some(observation) = observation else {
            self.candidate = None;
            return None;
        };
        let stable = self.candidate.as_ref() == Some(&observation);
        self.candidate = Some(observation.clone());
        self.incorporate_observation(&observation);
        if !stable || self.desired == self.persisted || self.queued.as_ref() == Some(&self.desired)
        {
            return None;
        }
        self.queued = Some(self.desired.clone());
        self.queued.clone()
    }

    fn mark_persisted(&mut self, preferences: AppPreferences) {
        if self.queued.as_ref() == Some(&preferences) {
            self.queued = None;
        }
        self.persisted = preferences;
    }

    fn snapshot_for_flush(&mut self, observation: Option<&WindowObservation>) -> AppPreferences {
        if let Some(observation) = observation {
            self.incorporate_observation(observation);
        }
        self.desired.clone()
    }

    #[cfg(test)]
    fn latest_desired(&self) -> &AppPreferences {
        &self.desired
    }
}

fn visible_page_name(page: &VisiblePage) -> &'static str {
    match page {
        VisiblePage::Home => "home",
        VisiblePage::Status => "status",
        VisiblePage::Subscriptions => "subscriptions",
        VisiblePage::Settings => "settings",
    }
}

fn visible_page_from_name(page: &str) -> Option<VisiblePage> {
    match page {
        "home" => Some(VisiblePage::Home),
        "status" => Some(VisiblePage::Status),
        "subscriptions" => Some(VisiblePage::Subscriptions),
        "settings" => Some(VisiblePage::Settings),
        _ => None,
    }
}

fn reduced_motion_from_client_area_animation(enabled: Option<bool>) -> bool {
    matches!(enabled, Some(false))
}

#[cfg(test)]
fn ambient_motion_allowed(
    user_enabled: bool,
    shell_active: bool,
    reduced_motion: bool,
    minimized: bool,
    home_visible: bool,
) -> bool {
    user_enabled && shell_active && !reduced_motion && !minimized && home_visible
}

fn shell_active_after_native_minimize_change(minimized: bool) -> bool {
    !minimized
}

#[cfg(windows)]
fn client_area_animation_enabled() -> Option<bool> {
    const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(
            action: u32,
            parameter: u32,
            value: *mut core::ffi::c_void,
            update: u32,
        ) -> i32;
    }

    let mut enabled = 1_i32;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL into the valid `enabled` pointer.
    let succeeded = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut enabled).cast(), 0)
    } != 0;
    succeeded.then_some(enabled != 0)
}

#[cfg(not(windows))]
fn client_area_animation_enabled() -> Option<bool> {
    None
}

const PREFERENCE_SAVE_ERROR_RU: &str = "Не удалось сохранить настройки.";
const PREFERENCE_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

fn retry_delay_after_failure(attempt: usize) -> Option<Duration> {
    attempt
        .checked_sub(1)
        .and_then(|index| PREFERENCE_RETRY_DELAYS.get(index).copied())
}

enum WriterCommand {
    Write(AppPreferences),
    Stop,
}

enum WriterResult {
    Saved(AppPreferences),
    Failed,
}

struct PreferenceWriter {
    command_tx: Sender<WriterCommand>,
    result_rx: Receiver<WriterResult>,
    handle: Option<thread::JoinHandle<()>>,
    store: Option<PreferenceStore>,
}

impl PreferenceWriter {
    fn start(store: Option<PreferenceStore>) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker_store = store.clone();
        let handle =
            thread::spawn(move || preference_writer_loop(worker_store, command_rx, result_tx));
        Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            store,
        }
    }

    fn enqueue(&self, preferences: AppPreferences) {
        let _ = self.command_tx.send(WriterCommand::Write(preferences));
    }

    fn poll(&self) -> Vec<WriterResult> {
        self.result_rx.try_iter().collect()
    }

    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.command_tx.send(WriterCommand::Stop);
            let _ = handle.join();
        }
    }

    fn stop_and_flush(&mut self, preferences: &AppPreferences) -> bool {
        self.stop();
        self.store
            .as_ref()
            .is_some_and(|store| store.save(preferences).is_ok())
    }
}

impl Drop for PreferenceWriter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn preference_writer_loop(
    store: Option<PreferenceStore>,
    commands: Receiver<WriterCommand>,
    results: Sender<WriterResult>,
) {
    while let Ok(command) = commands.recv() {
        let WriterCommand::Write(mut preferences) = command else {
            break;
        };
        let mut stop_after_write = drain_latest_write(&commands, &mut preferences);
        let mut attempt = 0_usize;
        loop {
            attempt += 1;
            let saved = store
                .as_ref()
                .is_some_and(|store| store.save(&preferences).is_ok());
            if saved {
                let _ = results.send(WriterResult::Saved(preferences));
                if stop_after_write {
                    return;
                }
                break;
            }
            let _ = results.send(WriterResult::Failed);
            if stop_after_write {
                return;
            }
            let Some(retry_delay) = retry_delay_after_failure(attempt) else {
                break;
            };
            match commands.recv_timeout(retry_delay) {
                Ok(WriterCommand::Write(replacement)) => {
                    preferences = replacement;
                    attempt = 0;
                    stop_after_write = drain_latest_write(&commands, &mut preferences);
                }
                Ok(WriterCommand::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

fn drain_latest_write(commands: &Receiver<WriterCommand>, latest: &mut AppPreferences) -> bool {
    loop {
        match commands.try_recv() {
            Ok(WriterCommand::Write(replacement)) => *latest = replacement,
            Ok(WriterCommand::Stop) | Err(TryRecvError::Disconnected) => return true,
            Err(TryRecvError::Empty) => return false,
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let launch_arguments =
        parse_launch_arguments(std::env::args_os().skip(1)).unwrap_or_else(|error| {
            eprintln!("launch arguments rejected: {error}");
            LaunchArguments::foreground()
        });
    let activation = activation_for_launch_arguments(&launch_arguments);
    let (_instance_guard, activations) = match single_instance::claim_or_forward(&activation) {
        Ok(single_instance::Claim::Primary {
            _guard,
            activations,
        }) => (_guard, activations),
        Ok(single_instance::Claim::Forwarded) => return Ok(()),
        Err(error) => {
            eprintln!("single-instance coordination failed: {error}");
            return Ok(());
        }
    };
    let launch_mode = launch_arguments.mode;
    let (client, _bootstrap_guard): (Arc<dyn DaemonClient>, Option<bootstrap::Bootstrap>) =
        match bootstrap::start() {
            Ok(bootstrap) => (bootstrap.client(), Some(bootstrap)),
            Err(error) => {
                eprintln!("bootstrap failed: {error:?}");
                let error = DaemonError::new(error.user_message());
                (Arc::new(UnavailableDaemonClient::new(error)), None)
            }
        };

    let (preference_store, loaded_preferences) = load_preferences();
    let preference_tracker = Arc::new(Mutex::new(PersistenceTracker::new(
        loaded_preferences.clone(),
    )));
    let preference_writer = Arc::new(Mutex::new(PreferenceWriter::start(preference_store)));

    let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
    let profile_preferences = Arc::new(Mutex::new(ProfilePreferenceReconciler::new(
        loaded_preferences.clone(),
        preference_tracker.clone(),
    )));
    let update_state = Arc::new(Mutex::new(UpdateState::initial()));

    let ui = AppWindow::new()?;
    let _logo_cache_session = logo_cache::LogoCacheSession::install();
    if preference_writer
        .lock()
        .expect("preference writer lock")
        .store
        .is_none()
    {
        ui.set_preference_error(PREFERENCE_SAVE_ERROR_RU.into());
    }
    apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
    ui.set_local_page(visible_page_name(&loaded_preferences.visible_page).into());
    ui.set_ambient_background_enabled(loaded_preferences.ambient_background);
    ui.set_reduced_motion(reduced_motion_from_client_area_animation(
        client_area_animation_enabled(),
    ));
    if let Some(subscription_url) = launch_arguments.subscription_url {
        ui.set_local_page("subscriptions".into());
        ui.set_subscription_draft(subscription_url.into());
        ui.set_replace_editor_open(true);
    }
    apply_update_state(
        &ui,
        &update_state.lock().expect("update state lock"),
        runtime_allows_update(&model.lock().expect("view model lock").presentation()),
    );
    apply_launch_at_sign_in(&ui, windows_settings::query_launch_at_sign_in());
    apply_saved_placement(&ui, &loaded_preferences);
    wire_import(&ui, model.clone(), profile_preferences.clone());
    wire_clipboard_import(&ui, model.clone());
    wire_subscription_refresh(&ui, model.clone(), profile_preferences.clone());
    let _subscription_refresh_timer =
        wire_automatic_subscription_refresh(&ui, model.clone(), profile_preferences.clone());
    wire_profile_switcher(&ui, model.clone(), profile_preferences.clone());
    wire_primary_action(&ui, model.clone(), profile_preferences.clone());
    wire_panels(&ui, model.clone());
    wire_event_filter(&ui, model.clone());
    wire_catalog_group(&ui, model.clone());
    wire_catalog_node(&ui, model.clone());
    let _latency_timer = wire_automatic_latency(&ui, model.clone());
    wire_windows_settings(&ui);
    wire_external_activations(&ui, activations, model.clone());
    let tray_popup = TrayPopup::new()?;
    let tray_popup_open = Rc::new(Cell::new(false));
    let tray_accordion = Rc::new(RefCell::new(TrayAccordionState::default()));
    wire_tray_popup_actions(
        &tray_popup,
        ui.as_weak(),
        model.clone(),
        preference_writer.clone(),
        preference_tracker.clone(),
        tray_popup_open.clone(),
        tray_accordion.clone(),
    );
    let _tray_popup_focus_timer = wire_tray_popup_focus_loss(
        &tray_popup,
        tray_popup_open.clone(),
        model.clone(),
        tray_accordion.clone(),
    );
    let tray_model = tray_menu_model(&model.lock().expect("view model lock"));
    let tray_model_source = model.clone();
    let tray_action_ui = ui.as_weak();
    let tray_action_model = model.clone();
    let tray_preference_writer = preference_writer.clone();
    let tray_preference_tracker = preference_tracker.clone();
    let tray_interaction_popup = tray_popup.as_weak();
    let tray_interaction_popup_open = tray_popup_open.clone();
    let tray_interaction_accordion = tray_accordion;
    let tray_runtime = tray::start(
        tray_model,
        move || tray_menu_model(&tray_model_source.lock().expect("view model lock")),
        move |interaction| match interaction {
            TrayInteraction::Command(command) => {
                hide_tray_popup(&tray_interaction_popup, &tray_interaction_popup_open);
                handle_tray_command(
                    &tray_action_ui,
                    &tray_action_model,
                    command,
                    &tray_preference_writer,
                    &tray_preference_tracker,
                )
            }
            TrayInteraction::ShowMenu { x, y } => {
                toggle_tray_popup(
                    &tray_interaction_popup,
                    &tray_action_ui,
                    &tray_action_model,
                    &tray_interaction_popup_open,
                    &tray_interaction_accordion,
                    PhysicalPosition { x, y },
                );
            }
        },
    )
    .map_err(|error| eprintln!("tray unavailable: {error}"))
    .ok();
    wire_window_controls(&ui, tray_runtime.is_some());
    let _preference_timer = wire_preference_persistence(
        &ui,
        model.clone(),
        preference_writer.clone(),
        preference_tracker.clone(),
    );
    wire_updater(&ui, model.clone(), update_state.clone());
    if updater::configured_repository().ok().flatten().is_some() {
        run_update_check(ui.as_weak(), model.clone(), update_state);
    }
    run_refresh(ui.as_weak(), model.clone(), profile_preferences);
    let starts_visible = initial_window_visible(launch_mode, tray_runtime.is_some());
    ui.set_shell_active(starts_visible);
    if starts_visible {
        ui.show()?;
    }
    let result = slint::run_event_loop();
    flush_preferences(&ui, &model, &preference_writer, &preference_tracker);
    let _ = tray_popup.hide();
    let _ = ui.hide();
    drop(tray_runtime);
    result
}

fn activation_for_launch_arguments(arguments: &LaunchArguments) -> single_instance::Activation {
    match (&arguments.subscription_url, arguments.mode) {
        (Some(url), _) => single_instance::Activation::InstallSubscription(url.clone()),
        (None, LaunchMode::Foreground) => single_instance::Activation::Show,
        (None, LaunchMode::Background) => single_instance::Activation::Background,
    }
}

fn prefill_subscription(ui: &AppWindow, subscription_url: String) {
    ui.set_local_page("subscriptions".into());
    ui.set_subscription_draft(subscription_url.into());
    ui.set_replace_editor_open(true);
}

fn wire_external_activations(
    ui: &AppWindow,
    activations: std::sync::mpsc::Receiver<single_instance::Activation>,
    model: Arc<Mutex<DesktopViewModel>>,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(activation) = activations.recv() {
            let weak = weak.clone();
            let model = model.clone();
            if slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match activation {
                    single_instance::Activation::Background => {}
                    single_instance::Activation::Show => restore_and_focus(&ui),
                    single_instance::Activation::InstallSubscription(url) => {
                        let current = {
                            let mut model = model.lock().expect("view model lock");
                            model.open_panel(Panel::None);
                            snapshot(&model)
                        };
                        apply_snapshot(&ui, &current);
                        prefill_subscription(&ui, url);
                        restore_and_focus(&ui);
                    }
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}

fn restore_and_focus(ui: &AppWindow) {
    ui.set_shell_active(true);
    ui.window().set_minimized(false);
    let _ = ui.show();
    let _ = ui.window().with_winit_window(|window| {
        window.set_minimized(false);
        window.set_visible(true);
        window.focus_window();
        window.request_redraw();
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchMode {
    Foreground,
    Background,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchArguments {
    mode: LaunchMode,
    subscription_url: Option<String>,
}

impl LaunchArguments {
    fn foreground() -> Self {
        Self {
            mode: LaunchMode::Foreground,
            subscription_url: None,
        }
    }
}

const INSTALL_SUBSCRIPTION_PREFIX: &str = "multicore://install-sub?url=";

fn parse_launch_arguments(
    args: impl IntoIterator<Item = OsString>,
) -> Result<LaunchArguments, &'static str> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(LaunchArguments::foreground()),
        [argument] if argument == OsStr::new("--quiet-signal-preview") => {
            Ok(LaunchArguments::foreground())
        }
        [argument] if argument == OsStr::new("--background") => Ok(LaunchArguments {
            mode: LaunchMode::Background,
            subscription_url: None,
        }),
        [argument] => {
            let argument = argument
                .to_str()
                .ok_or("launch argument must be valid Unicode")?;
            let encoded = argument
                .strip_prefix(INSTALL_SUBSCRIPTION_PREFIX)
                .ok_or("unsupported launch argument")?;
            let subscription_url = decode_percent_encoded_url(encoded)?;
            let parsed = reqwest::Url::parse(&subscription_url)
                .map_err(|_| "subscription URL is malformed")?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err("subscription URL must use HTTP or HTTPS");
            }
            Ok(LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some(subscription_url),
            })
        }
        _ => Err("background mode and installer URI cannot be combined"),
    }
}

fn decode_percent_encoded_url(encoded: &str) -> Result<String, &'static str> {
    if encoded.is_empty() || !encoded.as_bytes().contains(&b'%') {
        return Err("subscription URL must be percent-encoded");
    }

    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1).ok_or("incomplete percent escape")?;
                let low = *bytes.get(index + 2).ok_or("incomplete percent escape")?;
                decoded.push(
                    hex_value(high)
                        .and_then(|high| hex_value(low).map(|low| high * 16 + low))
                        .ok_or("invalid percent escape")?,
                );
                index += 3;
            }
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') => {
                decoded.push(byte);
                index += 1;
            }
            _ => return Err("subscription URL contains an unescaped character"),
        }
    }

    String::from_utf8(decoded).map_err(|_| "subscription URL is not valid UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn initial_window_visible(launch_mode: LaunchMode, tray_available: bool) -> bool {
    launch_mode == LaunchMode::Foreground || !tray_available
}

fn smoke_close_requested() -> bool {
    std::env::var_os("MULTICORE_SMOKE_EXIT_ON_CLOSE").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn runtime_allows_update(presentation: &StatePresentation) -> bool {
    !presentation.is_connected
        && !presentation.is_busy
        && !presentation.is_error
        && !presentation.is_degraded
}

fn apply_update_state(ui: &AppWindow, state: &UpdateState, runtime_idle: bool) {
    let presentation = state.presentation(runtime_idle);
    ui.set_update_status(presentation.status.into());
    ui.set_update_detail(presentation.detail.into());
    ui.set_update_action_label(presentation.action_label.into());
    ui.set_update_action_enabled(presentation.action_enabled);
    ui.set_update_available(presentation.available);
    ui.set_update_busy(presentation.busy);
}

fn load_preferences() -> (Option<PreferenceStore>, AppPreferences) {
    let store = match PreferenceStore::from_local_app_data() {
        Ok(store) => store,
        Err(_) => {
            eprintln!("preferences unavailable");
            return (None, AppPreferences::default());
        }
    };
    let preferences = match store.load() {
        Ok(preferences) => preferences,
        Err(_) => {
            eprintln!("preferences load failed");
            AppPreferences::default()
        }
    };
    (Some(store), preferences)
}

#[cfg(windows)]
fn monitor_work_area(
    monitor: &slint::winit_030::winit::monitor::MonitorHandle,
) -> Option<(
    slint::winit_030::winit::dpi::PhysicalPosition<i32>,
    slint::winit_030::winit::dpi::PhysicalSize<u32>,
)> {
    use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
    use slint::winit_030::winit::platform::windows::MonitorHandleExtWindows;

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }

    let mut info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        ..MonitorInfo::default()
    };
    // SAFETY: `info` has the Win32 MONITORINFO layout and remains writable for the call.
    if unsafe { GetMonitorInfoW(monitor.hmonitor(), &raw mut info) } == 0 {
        return None;
    }
    let width = u32::try_from(info.work.right.checked_sub(info.work.left)?).ok()?;
    let height = u32::try_from(info.work.bottom.checked_sub(info.work.top)?).ok()?;
    (width > 0 && height > 0).then_some((
        PhysicalPosition::new(info.work.left, info.work.top),
        PhysicalSize::new(width, height),
    ))
}

#[cfg(not(windows))]
fn monitor_work_area(
    monitor: &slint::winit_030::winit::monitor::MonitorHandle,
) -> Option<(
    slint::winit_030::winit::dpi::PhysicalPosition<i32>,
    slint::winit_030::winit::dpi::PhysicalSize<u32>,
)> {
    Some((monitor.position(), monitor.size()))
}

fn monitor_rects(window: &slint::winit_030::winit::window::Window) -> Vec<MonitorRect> {
    let primary = window.primary_monitor();
    window
        .available_monitors()
        .filter_map(|monitor| {
            let scale = monitor.scale_factor();
            let scale_milli = scale_factor_milli(scale)?;
            let (position, size) = monitor_work_area(&monitor)?;
            Some(MonitorRect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
                scale_milli,
                primary: primary.as_ref() == Some(&monitor),
            })
        })
        .collect()
}

fn scale_factor_milli(scale: f64) -> Option<u32> {
    if !scale.is_finite() || !(0.5..=8.0).contains(&scale) {
        return None;
    }
    Some((scale * 1_000.0).round() as u32)
}

fn current_monitor_signature(
    window: &slint::winit_030::winit::window::Window,
) -> Option<MonitorSignature> {
    let monitor = window.current_monitor()?;
    let (position, size) = monitor_work_area(&monitor)?;
    Some(MonitorSignature {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        scale_milli: scale_factor_milli(monitor.scale_factor())?,
    })
}

fn set_current_monitor_max_size(
    window: &slint::winit_030::winit::window::Window,
) -> Option<MonitorSignature> {
    use slint::winit_030::winit::dpi::LogicalSize;

    let signature = current_monitor_signature(window);
    let maximum = signature.map(|signature| {
        let width = u64::from(signature.width) * 1_000 / u64::from(signature.scale_milli);
        let height = u64::from(signature.height) * 1_000 / u64::from(signature.scale_milli);
        LogicalSize::new(
            width.min(u64::from(u32::MAX)) as u32,
            height.min(u64::from(u32::MAX)) as u32,
        )
    });
    window.set_max_inner_size(maximum);
    signature
}

fn apply_saved_placement(ui: &AppWindow, preferences: &AppPreferences) {
    use slint::winit_030::winit::dpi::{LogicalSize, PhysicalPosition};

    let saved = preferences.restored_bounds.clone();
    let maximized = preferences.maximized;
    let _ = ui.window().with_winit_window(|window| {
        let monitors = monitor_rects(window);
        let mut restored = false;
        if let Some(placement) = saved.and_then(|saved| restore_placement(saved, &monitors)) {
            let _ = window.request_inner_size(LogicalSize::new(
                placement.inner_size.width,
                placement.inner_size.height,
            ));
            window.set_outer_position(PhysicalPosition::new(
                placement.position.x,
                placement.position.y,
            ));
            window.set_max_inner_size(Some(LogicalSize::new(
                placement.max_inner_size.width,
                placement.max_inner_size.height,
            )));
            restored = true;
        }
        if !restored {
            let _ = set_current_monitor_max_size(window);
        }
        window.set_maximized(maximized);
    });
}

fn observe_window(ui: &AppWindow) -> Option<WindowObservation> {
    let visible_page = visible_page_from_name(ui.get_local_page().as_str())?;
    ui.window().with_winit_window(|window| {
        if window.is_minimized() == Some(true) {
            return None;
        }
        let maximized = window.is_maximized();
        let restored_bounds = if maximized {
            None
        } else {
            let scale = window.scale_factor();
            let position = window.outer_position().ok()?;
            let size = window.inner_size().to_logical::<u32>(scale);
            Some(WindowBounds {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            })
        };
        Some(WindowObservation {
            restored_bounds,
            maximized,
            visible_page,
        })
    })?
}

fn process_writer_results(
    ui: &AppWindow,
    writer: &PreferenceWriter,
    tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    for result in writer.poll() {
        ui.set_preference_error(preference_error_for_result(&result).into());
        match result {
            WriterResult::Saved(preferences) => {
                tracker
                    .lock()
                    .expect("preference tracker lock")
                    .mark_persisted(preferences);
            }
            WriterResult::Failed => {
                eprintln!("preferences save failed");
            }
        }
    }
}

fn preference_error_for_result(result: &WriterResult) -> &'static str {
    match result {
        WriterResult::Saved(_) => "",
        WriterResult::Failed => PREFERENCE_SAVE_ERROR_RU,
    }
}

fn flush_preferences(
    ui: &AppWindow,
    model: &Arc<Mutex<DesktopViewModel>>,
    writer: &Arc<Mutex<PreferenceWriter>>,
    tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    let observation = observe_window(ui);
    let route_preferences = model.lock().expect("view model lock").route_preferences();
    let preferences = {
        let mut tracker = tracker.lock().expect("preference tracker lock");
        tracker.set_ambient_background(ui.get_ambient_background_enabled());
        if let Some((profile, selected_group, selections)) = route_preferences {
            tracker.set_route_preferences(profile, selected_group, selections);
        }
        tracker.snapshot_for_flush(observation.as_ref())
    };
    let saved = writer
        .lock()
        .expect("preference writer lock")
        .stop_and_flush(&preferences);
    if saved {
        tracker
            .lock()
            .expect("preference tracker lock")
            .mark_persisted(preferences);
        ui.set_preference_error("".into());
    } else {
        ui.set_preference_error(PREFERENCE_SAVE_ERROR_RU.into());
        eprintln!("preferences save failed");
    }
}

fn wire_preference_persistence(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    writer: Arc<Mutex<PreferenceWriter>>,
    tracker: Arc<Mutex<PersistenceTracker>>,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let last_monitor_signature = Cell::new(None);
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(current) = ui.window().with_winit_window(current_monitor_signature) {
            let previous = last_monitor_signature.get();
            if monitor_cap_changed(previous, current)
                && let Some(applied) = ui.window().with_winit_window(set_current_monitor_max_size)
            {
                last_monitor_signature.set(applied);
            }
        }
        process_writer_results(
            &ui,
            &writer.lock().expect("preference writer lock"),
            &tracker,
        );
        let observation = observe_window(&ui);
        let route_preferences = model.lock().expect("view model lock").route_preferences();
        let preferences = {
            let mut tracker = tracker.lock().expect("preference tracker lock");
            tracker.set_ambient_background(ui.get_ambient_background_enabled());
            if let Some((profile, selected_group, selections)) = route_preferences {
                tracker.set_route_preferences(profile, selected_group, selections);
            }
            tracker.observe(observation)
        };
        if let Some(preferences) = preferences {
            writer
                .lock()
                .expect("preference writer lock")
                .enqueue(preferences);
        }
    });
    timer
}

fn wire_updater(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
) {
    let weak = ui.as_weak();
    ui.on_update_action(move || {
        let runtime_idle =
            runtime_allows_update(&model.lock().expect("view model lock").presentation());
        let action = state
            .lock()
            .expect("update state lock")
            .presentation(runtime_idle)
            .action;
        match action {
            UpdateAction::Check => run_update_check(weak.clone(), model.clone(), state.clone()),
            UpdateAction::Install if runtime_idle => {
                let release = state.lock().expect("update state lock").available_release();
                let Some(release) = release else {
                    return;
                };
                {
                    let mut current = state.lock().expect("update state lock");
                    *current = UpdateState::Downloading(release.clone());
                    if let Some(ui) = weak.upgrade() {
                        apply_update_state(&ui, &current, true);
                    }
                }
                let weak = weak.clone();
                let state = state.clone();
                thread::spawn(move || match updater::download_and_start_apply(&release) {
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            let _keep_state_alive = state;
                            let _keep_window_alive = weak;
                            let _ = slint::quit_event_loop();
                        });
                    }
                    Err(error) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            let mut current = state.lock().expect("update state lock");
                            *current = UpdateState::Error(error);
                            if let Some(ui) = weak.upgrade() {
                                apply_update_state(&ui, &current, true);
                            }
                        });
                    }
                });
            }
            UpdateAction::None | UpdateAction::Install => {}
        }
    });
}

fn run_update_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    state: Arc<Mutex<UpdateState>>,
) {
    {
        let mut current = state.lock().expect("update state lock");
        *current = UpdateState::Checking;
        if let Some(ui) = weak.upgrade() {
            let runtime_idle =
                runtime_allows_update(&model.lock().expect("view model lock").presentation());
            apply_update_state(&ui, &current, runtime_idle);
        }
    }
    thread::spawn(move || {
        let result = updater::check_latest().unwrap_or_else(UpdateState::Error);
        let _ = slint::invoke_from_event_loop(move || {
            let runtime_idle =
                runtime_allows_update(&model.lock().expect("view model lock").presentation());
            let mut current = state.lock().expect("update state lock");
            *current = result;
            if let Some(ui) = weak.upgrade() {
                apply_update_state(&ui, &current, runtime_idle);
            }
        });
    });
}

fn wire_window_controls(ui: &AppWindow, tray_available: bool) {
    let weak = ui.as_weak();
    ui.on_window_drag(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| window.drag_window());
        }
    });

    let weak = ui.as_weak();
    ui.on_window_minimize(move || match minimize_disposition(tray_available) {
        MinimizeDisposition::HideToTray => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                hide_window_to_tray(&ui);
            }
        }
        MinimizeDisposition::MinimizeToTaskbar => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(shell_active_after_native_minimize_change(true));
                ui.window().set_minimized(true);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_window_resize(move |edge| {
        let Some(edge) = parse_resize_edge(edge.as_str()) else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| {
                let _ = set_current_monitor_max_size(window);
                #[cfg(windows)]
                let _ = window.drag_resize_window(edge.into());
                #[cfg(not(windows))]
                let _ = edge;
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_window_toggle_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            let _ = window.with_winit_window(set_current_monitor_max_size);
            window.set_maximized(next_maximized(window.is_maximized()));
        }
    });

    let weak = ui.as_weak();
    ui.on_window_close(move || {
        close_window(&weak, tray_available);
    });

    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        close_window(&weak, tray_available);
        CloseRequestResponse::KeepWindowShown
    });
}

fn close_window(weak: &slint::Weak<AppWindow>, tray_available: bool) {
    match close_disposition(tray_available, smoke_close_requested()) {
        CloseDisposition::HideToTray => {
            if let Some(ui) = weak.upgrade() {
                ui.set_shell_active(false);
                hide_window_to_tray(&ui);
            }
        }
        CloseDisposition::Exit => {
            let _ = slint::quit_event_loop();
        }
    }
}

/// Hide the native surface without deregistering the Slint window.
///
/// `slint::Window::hide()` releases the framework keep-alive for the window.
/// When the tray is the only remaining recovery surface that lets the event
/// loop terminate as soon as the title bar is closed/minimized. Keep the
/// component registered and only hide its Winit window instead; tray events
/// and timers can then continue to run until the explicit tray Exit action.
fn hide_window_to_tray(ui: &AppWindow) {
    let _ = ui.window().with_winit_window(|window| {
        window.set_visible(false);
    });
}

fn apply_tray_popup_model(
    popup: &TrayPopup,
    model: TrayMenuModel,
    accordion: &mut TrayAccordionState,
) {
    let nodes = model
        .nodes
        .iter()
        .take(64)
        .filter_map(|entry| match &entry.command {
            TrayCommand::SelectNode { group_id, node_id } => Some(TrayMenuRow {
                id: node_id.clone().into(),
                group_id: group_id.clone().into(),
                icon: entry.icon.clone().into(),
                label: entry.label.clone().into(),
                detail: "".into(),
                selected: entry.checked,
                enabled: entry.enabled,
                expanded: false,
                node_count: 0,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    accordion.retain_available(
        model
            .groups
            .iter()
            .filter_map(|entry| match &entry.command {
                TrayCommand::SelectGroup { group_id } => Some(group_id.as_str()),
                _ => None,
            }),
    );
    let groups = model
        .groups
        .iter()
        .take(12)
        .filter_map(|entry| match &entry.command {
            TrayCommand::SelectGroup { group_id } => Some(TrayMenuRow {
                id: group_id.clone().into(),
                group_id: "".into(),
                icon: entry.icon.clone().into(),
                label: entry.label.clone().into(),
                detail: entry.detail.clone().into(),
                selected: false,
                enabled: entry.enabled,
                expanded: accordion.is_expanded(group_id),
                node_count: nodes
                    .iter()
                    .filter(|node| node.group_id.as_str() == group_id)
                    .count() as i32,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();

    popup.set_status_text(model.status.into());
    popup.set_connection_label(model.connection_label.into());
    popup.set_connection_enabled(model.connection_enabled);
    popup.set_group_rows(ModelRc::new(VecModel::from(groups)));
    popup.set_node_rows(ModelRc::new(VecModel::from(nodes)));
}

fn hide_tray_popup(weak: &slint::Weak<TrayPopup>, open: &Rc<Cell<bool>>) {
    if let Some(popup) = weak.upgrade() {
        let _ = popup.hide();
    }
    open.set(false);
}

fn toggle_tray_popup(
    popup_weak: &slint::Weak<TrayPopup>,
    main_weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    open: &Rc<Cell<bool>>,
    accordion: &Rc<RefCell<TrayAccordionState>>,
    anchor: PhysicalPosition,
) {
    if open.get() {
        hide_tray_popup(popup_weak, open);
        return;
    }
    let (Some(popup), Some(main)) = (popup_weak.upgrade(), main_weak.upgrade()) else {
        return;
    };
    apply_tray_popup_model(
        &popup,
        tray_menu_model(&model.lock().expect("view model lock")),
        &mut accordion.borrow_mut(),
    );
    let _ = popup.show();
    let monitors = main
        .window()
        .with_winit_window(monitor_rects)
        .unwrap_or_default();
    let popup_size = popup
        .window()
        .with_winit_window(|window| window.outer_size())
        .map(|size| (size.width, size.height))
        .unwrap_or((304, 460));
    if let Some(position) = place_tray_popup(anchor, popup_size, &monitors, None) {
        let _ = popup.window().with_winit_window(|window| {
            use slint::winit_030::winit::dpi::PhysicalPosition as WinitPhysicalPosition;
            use slint::winit_030::winit::window::WindowLevel;
            apply_tray_popup_native_rounding(window);
            window.set_window_level(WindowLevel::AlwaysOnTop);
            window.set_outer_position(WinitPhysicalPosition::new(position.x, position.y));
            window.focus_window();
            window.request_redraw();
        });
    }
    open.set(true);
}

fn dispatch_tray_popup_command(
    popup: &slint::Weak<TrayPopup>,
    open: &Rc<Cell<bool>>,
    ui: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    hide_tray_popup(popup, open);
    handle_tray_command(ui, model, command, preference_writer, preference_tracker);
}

fn dispatch_tray_popup_selection(
    popup: &slint::Weak<TrayPopup>,
    ui: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
    accordion: &Rc<RefCell<TrayAccordionState>>,
) {
    handle_tray_command(ui, model, command, preference_writer, preference_tracker);
    if let Some(popup) = popup.upgrade() {
        apply_tray_popup_model(
            &popup,
            tray_menu_model(&model.lock().expect("view model lock")),
            &mut accordion.borrow_mut(),
        );
    }
}

fn dispatch_tray_popup_group_toggle(
    popup: &slint::Weak<TrayPopup>,
    ui: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    group_id: String,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
    accordion: &Rc<RefCell<TrayAccordionState>>,
) {
    let expanded = accordion.borrow_mut().toggle(&group_id);
    if expanded {
        handle_tray_command(
            ui,
            model,
            TrayCommand::SelectGroup {
                group_id: group_id.clone(),
            },
            preference_writer,
            preference_tracker,
        );
    }
    if let Some(popup) = popup.upgrade() {
        apply_tray_popup_model(
            &popup,
            tray_menu_model(&model.lock().expect("view model lock")),
            &mut accordion.borrow_mut(),
        );
    }
}

fn wire_tray_popup_actions(
    popup: &TrayPopup,
    ui: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    preference_writer: Arc<Mutex<PreferenceWriter>>,
    preference_tracker: Arc<Mutex<PersistenceTracker>>,
    open: Rc<Cell<bool>>,
    accordion: Rc<RefCell<TrayAccordionState>>,
) {
    let popup_weak = popup.as_weak();
    let dismiss_popup = popup_weak.clone();
    let dismiss_open = open.clone();
    popup.on_dismiss(move || hide_tray_popup(&dismiss_popup, &dismiss_open));

    let wire_command = |command: TrayCommand| {
        let popup = popup_weak.clone();
        let open = open.clone();
        let ui = ui.clone();
        let model = model.clone();
        let writer = preference_writer.clone();
        let tracker = preference_tracker.clone();
        move || {
            dispatch_tray_popup_command(
                &popup,
                &open,
                &ui,
                &model,
                command.clone(),
                &writer,
                &tracker,
            );
        }
    };
    popup.on_toggle_connection(wire_command(TrayCommand::ToggleConnection));
    popup.on_open_main(wire_command(TrayCommand::ShowWindow));
    popup.on_exit_app(wire_command(TrayCommand::Exit));

    let group_popup = popup_weak.clone();
    let group_ui = ui.clone();
    let group_model = model.clone();
    let group_writer = preference_writer.clone();
    let group_tracker = preference_tracker.clone();
    let group_accordion = accordion.clone();
    popup.on_toggle_group(move |group_id| {
        dispatch_tray_popup_group_toggle(
            &group_popup,
            &group_ui,
            &group_model,
            group_id.to_string(),
            &group_writer,
            &group_tracker,
            &group_accordion,
        );
    });

    let node_popup = popup_weak;
    let node_accordion = accordion;
    popup.on_select_node(move |group_id, node_id| {
        dispatch_tray_popup_selection(
            &node_popup,
            &ui,
            &model,
            TrayCommand::SelectNode {
                group_id: group_id.to_string(),
                node_id: node_id.to_string(),
            },
            &preference_writer,
            &preference_tracker,
            &node_accordion,
        );
    });
}

fn wire_tray_popup_focus_loss(
    popup: &TrayPopup,
    open: Rc<Cell<bool>>,
    model: Arc<Mutex<DesktopViewModel>>,
    accordion: Rc<RefCell<TrayAccordionState>>,
) -> Timer {
    let timer = Timer::default();
    let weak = popup.as_weak();
    let grace_ticks = Rc::new(Cell::new(0_u8));
    let timer_grace = grace_ticks.clone();
    let mut initial_model_cache = TrayPopupModelCache::default();
    initial_model_cache.observe(&tray_menu_model(&model.lock().expect("view model lock")));
    let model_cache = Rc::new(RefCell::new(initial_model_cache));
    timer.start(TimerMode::Repeated, Duration::from_millis(120), move || {
        if !open.get() {
            timer_grace.set(2);
            return;
        }
        if timer_grace.get() > 0 {
            timer_grace.set(timer_grace.get() - 1);
            return;
        }
        let Some(popup) = weak.upgrade() else {
            return;
        };
        let next_model = tray_menu_model(&model.lock().expect("view model lock"));
        if model_cache.borrow_mut().observe(&next_model) {
            apply_tray_popup_model(&popup, next_model, &mut accordion.borrow_mut());
        }
        let focused = popup
            .window()
            .with_winit_window(|window| window.has_focus())
            .unwrap_or(false);
        if !focused {
            let _ = popup.hide();
            open.set(false);
        }
    });
    timer
}

fn tray_menu_model(model: &DesktopViewModel) -> TrayMenuModel {
    let presentation = model.presentation();
    let catalog = model.catalog_presentation();
    let tray_groups = model.tray_catalog_groups(12, 64);
    let status = if presentation.is_connected {
        format!("В сети · {}", presentation.node)
    } else {
        presentation.headline.clone()
    };
    build_menu_model(TrayInput {
        status,
        connection_label: presentation.primary_label.into(),
        connection_enabled: presentation.primary_enabled
            && presentation.primary_action != PrimaryAction::None,
        groups: tray_groups
            .into_iter()
            .map(|group| {
                let group_display = derive_catalog_display(&group.label, CatalogDisplayKind::Group);
                let selected_node =
                    derive_catalog_display(&group.selected_node_label, CatalogDisplayKind::Node);
                let nodes = group
                    .nodes
                    .into_iter()
                    .map(|node| {
                        let display = derive_catalog_display(&node.label, CatalogDisplayKind::Node);
                        TrayNode {
                            id: node.id,
                            label: display.label,
                            icon: display.icon,
                            selected: node.selected,
                        }
                    })
                    .collect();
                TrayGroup {
                    id: group.id,
                    label: group_display.label,
                    icon: group_display.icon,
                    selected_node_label: selected_node.label,
                    nodes,
                }
            })
            .collect(),
        route_selection_enabled: catalog.selection_enabled,
    })
}

fn handle_tray_command(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    command: TrayCommand,
    preference_writer: &Arc<Mutex<PreferenceWriter>>,
    preference_tracker: &Arc<Mutex<PersistenceTracker>>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    match command {
        TrayCommand::ShowWindow => {
            ui.set_shell_active(true);
            ui.window().set_minimized(false);
            let _ = ui.show();
            let _ = ui.window().with_winit_window(|window| {
                window.set_minimized(false);
                window.set_visible(true);
                window.focus_window();
                window.request_redraw();
            });
        }
        TrayCommand::ToggleConnection => ui.invoke_primary_action(),
        TrayCommand::SelectGroup { group_id } => {
            ui.invoke_select_catalog_group(group_id.into());
        }
        TrayCommand::SelectNode { group_id, node_id } => {
            let selected_group = model
                .lock()
                .expect("view model lock")
                .catalog_presentation()
                .selected_group_id;
            if selected_group.as_deref() != Some(group_id.as_str()) {
                ui.invoke_select_catalog_group(group_id.into());
            }
            ui.invoke_select_catalog_node(node_id.into());
        }
        TrayCommand::Exit => {
            flush_preferences(&ui, model, preference_writer, preference_tracker);
            let _ = slint::quit_event_loop();
        }
    }
}

fn wire_windows_settings(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_set_launch_at_sign_in(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            apply_launch_at_sign_in(&ui, windows_settings::set_launch_at_sign_in(enabled));
        }
    });
}

fn apply_launch_at_sign_in(ui: &AppWindow, state: std::io::Result<LaunchAtSignInState>) {
    match state {
        Ok(LaunchAtSignInState::Enabled) => {
            ui.set_launch_at_sign_in_enabled(true);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error("".into());
        }
        Ok(LaunchAtSignInState::Disabled) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error("".into());
        }
        Ok(LaunchAtSignInState::Stale { .. }) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(true);
            ui.set_launch_at_sign_in_error("".into());
        }
        Err(_) => {
            ui.set_launch_at_sign_in_enabled(false);
            ui.set_launch_at_sign_in_stale(false);
            ui.set_launch_at_sign_in_error(
                "Не удалось изменить автозапуск. Проверьте права пользователя.".into(),
            );
        }
    }
}

fn wire_import(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let weak = ui.as_weak();
    ui.on_import_url(move |url| {
        let url = url.trim().to_owned();
        let (mutation, client, current) = {
            let mut model = model.lock().expect("view model lock");
            profile_preferences
                .lock()
                .expect("profile preference reconciler lock")
                .capture_current(&model);
            let mutation = model.begin_import(&url);
            let client = model.daemon_client();
            (mutation, client, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        let Some(mutation) = mutation else {
            return;
        };

        run_import(
            weak.clone(),
            model.clone(),
            client,
            mutation,
            url,
            profile_preferences.clone(),
        );
    });
}

fn wire_clipboard_import(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let paste_model = model.clone();
    ui.on_paste_subscription(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match clipboard_text::read_subscription_text() {
            Ok(url) => {
                ui.set_subscription_draft(url.clone().into());
                ui.invoke_import_url(url.into());
            }
            Err(error) => {
                let current = {
                    let mut model = paste_model.lock().expect("view model lock");
                    model.set_import_error_message(error.user_message());
                    snapshot(&model)
                };
                apply_snapshot(&ui, &current);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_dismiss_first_run_import(move || {
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.dismiss_import_error();
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });
}

fn wire_subscription_refresh(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let weak = ui.as_weak();
    ui.on_refresh_subscription(move || {
        let (mutation, client, current, needs_url) = {
            let mut model = model.lock().expect("view model lock");
            let available = model.subscription_presentation().refresh_available;
            let mutation = model.begin_subscription_refresh();
            (
                mutation,
                model.daemon_client(),
                snapshot(&model),
                !available,
            )
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            if needs_url {
                ui.set_replace_editor_open(true);
            }
        }
        let Some(mutation) = mutation else {
            return;
        };
        run_subscription_refresh(
            weak.clone(),
            model.clone(),
            client,
            mutation,
            profile_preferences.clone(),
        );
    });
}

fn wire_automatic_subscription_refresh(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) -> Timer {
    const RETRY_BACKOFF_SECS: u64 = 15 * 60;

    let timer = Timer::default();
    let weak = ui.as_weak();
    let retry_after = Rc::new(Cell::new(0_u64));
    timer.start(TimerMode::Repeated, Duration::from_secs(60), move || {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (mutation, client, current) = {
            let mut model = model.lock().expect("view model lock");
            let due = now_unix >= retry_after.get()
                && model.automatic_subscription_refresh_due_at(now_unix);
            let mutation = due.then(|| model.begin_subscription_refresh()).flatten();
            (mutation, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(mutation) = mutation {
            retry_after.set(now_unix.saturating_add(RETRY_BACKOFF_SECS));
            run_subscription_refresh(
                weak.clone(),
                model.clone(),
                client,
                mutation,
                profile_preferences.clone(),
            );
        }
    });
    timer
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteFocusTarget {
    ManagementProfile(String),
    AddSubscription,
    DeleteControl(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeleteFocusPlan {
    deleting_profile_id: String,
    successful_target: DeleteFocusTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteFocusState {
    AwaitingCompletion(DeleteFocusPlan),
    Immediate(DeleteFocusTarget),
}

impl DeleteFocusPlan {
    fn completion(&self, succeeded: bool) -> DeleteFocusTarget {
        if succeeded {
            self.successful_target.clone()
        } else {
            DeleteFocusTarget::DeleteControl(self.deleting_profile_id.clone())
        }
    }
}

fn delete_focus_plan(
    profile_ids: &[&str],
    deleting_profile_id: &str,
    accepted: bool,
) -> DeleteFocusState {
    if !accepted {
        return DeleteFocusState::Immediate(DeleteFocusTarget::DeleteControl(
            deleting_profile_id.to_owned(),
        ));
    }
    let successful_target = profile_ids
        .iter()
        .position(|profile_id| *profile_id == deleting_profile_id)
        .and_then(|deleting_index| {
            profile_ids
                .get(deleting_index + 1)
                .or_else(|| {
                    deleting_index
                        .checked_sub(1)
                        .and_then(|index| profile_ids.get(index))
                })
                .copied()
        })
        .map(|profile_id| DeleteFocusTarget::ManagementProfile(profile_id.to_owned()))
        .unwrap_or(DeleteFocusTarget::AddSubscription);
    DeleteFocusState::AwaitingCompletion(DeleteFocusPlan {
        deleting_profile_id: deleting_profile_id.to_owned(),
        successful_target,
    })
}

fn apply_delete_focus_completion(ui: &AppWindow, completion: DeleteFocusTarget) {
    match completion {
        DeleteFocusTarget::ManagementProfile(profile_id) => {
            ui.set_profile_edit_id("".into());
            ui.set_profile_edit_draft("".into());
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_management_focus_id(profile_id.into());
            ui.set_profile_management_focus_serial(
                ui.get_profile_management_focus_serial().saturating_add(1),
            );
        }
        DeleteFocusTarget::AddSubscription => {
            ui.set_profile_edit_id("".into());
            ui.set_profile_edit_draft("".into());
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_management_focus_id("".into());
            ui.set_profile_management_focus_serial(
                ui.get_profile_management_focus_serial().saturating_add(1),
            );
        }
        DeleteFocusTarget::DeleteControl(profile_id) => {
            ui.set_profile_delete_confirm_id("".into());
            ui.set_profile_delete_control_focus_id(profile_id.into());
            ui.set_profile_delete_control_focus_serial(
                ui.get_profile_delete_control_focus_serial()
                    .saturating_add(1),
            );
        }
    }
}

fn wire_profile_switcher(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let weak = ui.as_weak();
    let open_model = model.clone();
    let open_preferences = profile_preferences.clone();
    ui.on_open_profile_menu(move || {
        let current = {
            let mut model = open_model.lock().expect("view model lock");
            model.open_panel(Panel::Subscription);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        load_profile_list(
            weak.clone(),
            open_model.clone(),
            open_preferences.clone(),
            false,
        );
    });

    let weak = ui.as_weak();
    let retry_model = model.clone();
    let retry_preferences = profile_preferences.clone();
    ui.on_retry_profile_menu(move || {
        if weak.upgrade().is_some_and(|ui| {
            ui.get_active_panel() == "subscription" || ui.get_local_page() == "subscriptions"
        }) {
            load_profile_list(
                weak.clone(),
                retry_model.clone(),
                retry_preferences.clone(),
                false,
            );
        }
    });

    let weak = ui.as_weak();
    let close_model = model.clone();
    ui.on_close_profile_menu(move || {
        let current = {
            let mut model = close_model.lock().expect("view model lock");
            model.open_panel(Panel::None);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
    });

    let weak = ui.as_weak();
    let add_model = model.clone();
    ui.on_add_subscription(move || {
        let current = {
            let mut model = add_model.lock().expect("view model lock");
            model.open_panel(Panel::None);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            ui.set_local_page("subscriptions".into());
            ui.set_subscription_draft("".into());
            ui.set_replace_editor_open(true);
        }
    });

    let weak = ui.as_weak();
    let activate_model = model.clone();
    let activate_preferences = profile_preferences.clone();
    ui.on_activate_profile(move |profile_id| {
        let (token, client, current) = {
            let mut model = activate_model.lock().expect("view model lock");
            activate_preferences
                .lock()
                .expect("profile preference reconciler lock")
                .capture_current(&model);
            let token = model.begin_profile_activation(profile_id.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_activation(
                weak.clone(),
                activate_model.clone(),
                activate_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let refresh_model = model.clone();
    let refresh_preferences = profile_preferences.clone();
    ui.on_refresh_profile(move |profile_id| {
        let (token, client, current) = {
            let mut model = refresh_model.lock().expect("view model lock");
            let token = model.begin_profile_target_refresh(profile_id.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_target_refresh(
                weak.clone(),
                refresh_model.clone(),
                refresh_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let edit_model = model.clone();
    let edit_preferences = profile_preferences.clone();
    ui.on_replace_profile_source(move |profile_id, url| {
        let (token, client, current) = {
            let mut model = edit_model.lock().expect("view model lock");
            let token = model.begin_profile_source_edit(profile_id.as_str(), url.as_str());
            (token, model.daemon_client(), snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        if let Some(token) = token {
            run_profile_source_edit(
                weak.clone(),
                edit_model.clone(),
                edit_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
                url.to_string(),
            );
        }
    });

    let weak = ui.as_weak();
    let delete_model = model;
    ui.on_delete_profile(move |profile_id| {
        let (token, client, current, focus_state) = {
            let mut model = delete_model.lock().expect("view model lock");
            let profiles = model.profile_presentation().profiles;
            let profile_ids = profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>();
            let token = model.begin_profile_delete(profile_id.as_str());
            let focus_state = delete_focus_plan(&profile_ids, profile_id.as_str(), token.is_some());
            (token, model.daemon_client(), snapshot(&model), focus_state)
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            match &focus_state {
                DeleteFocusState::AwaitingCompletion(_) => {
                    ui.set_profile_delete_confirm_id("".into());
                }
                DeleteFocusState::Immediate(completion) => {
                    apply_delete_focus_completion(&ui, completion.clone());
                }
            }
        }
        if let (Some(token), DeleteFocusState::AwaitingCompletion(focus_plan)) =
            (token, focus_state)
        {
            run_profile_delete(
                weak.clone(),
                delete_model.clone(),
                profile_preferences.clone(),
                client,
                token,
                profile_id.to_string(),
                focus_plan,
            );
        }
    });
}

fn wire_primary_action(
    ui: &AppWindow,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let weak = ui.as_weak();
    ui.on_primary_action(move || {
        let (action, client, queued, current) = {
            let mut model = model.lock().expect("view model lock");
            let action = model.begin_primary_action();
            let client = model.daemon_client();
            let queued = model.queued_node_selections();
            (action, client, queued, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }

        match action {
            Some((PrimaryAction::Connect, mutation)) => {
                run_connect(weak.clone(), model.clone(), client, mutation, queued);
            }
            Some((PrimaryAction::Disconnect | PrimaryAction::Cleanup, mutation)) => {
                run_disconnect(weak.clone(), model.clone(), client, mutation);
            }
            Some((PrimaryAction::Retry, mutation)) => {
                run_status(
                    weak.clone(),
                    model.clone(),
                    client,
                    mutation,
                    profile_preferences.clone(),
                );
            }
            Some((PrimaryAction::None, _)) | None => {}
        }
    });
}

fn wire_panels(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    let open_model = model.clone();
    ui.on_open_panel(move |name| {
        let panel = match name.as_str() {
            "subscription" => Panel::Subscription,
            "routes" => Panel::Routes,
            "events" => Panel::Events,
            _ => Panel::None,
        };
        let current = {
            let mut model = open_model.lock().expect("view model lock");
            model.open_panel(panel);
            snapshot(&model)
        };
        if let Some(ui) = weak.upgrade() {
            if panel == Panel::Events {
                ui.set_event_filter("Все".into());
            } else if panel == Panel::Routes {
                ui.set_local_page("home".into());
            }
            apply_snapshot(&ui, &current);
        }
        if panel == Panel::Events {
            let cached = open_model
                .lock()
                .expect("view model lock")
                .set_event_filter(None);
            if let Some(ui) = weak.upgrade() {
                apply_events(&ui, cached);
            }
            load_events(weak.clone(), open_model.clone());
            load_diagnostics(weak.clone(), open_model.clone());
        } else if panel == Panel::Routes {
            load_catalog(weak.clone(), open_model.clone());
        }
    });

    let weak = ui.as_weak();
    let close_model = model.clone();
    ui.on_close_panel(move || {
        close_panel(&weak, &close_model, false);
    });

    let weak = ui.as_weak();
    ui.on_escape_pressed(move || {
        close_panel(&weak, &model, true);
    });
}

fn wire_catalog_group(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_select_catalog_group(move |id| {
        let current = {
            let mut model = model.lock().expect("view model lock");
            model
                .select_catalog_group(id.as_str())
                .then(|| snapshot(&model))
        };
        if let (Some(ui), Some(current)) = (weak.upgrade(), current) {
            apply_snapshot(&ui, &current);
            begin_latency_check(weak.clone(), model.clone());
        }
    });
}

fn wire_catalog_node(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_select_catalog_node(move |id| {
        let (request, current) = {
            let mut model = model.lock().expect("view model lock");
            let request = model.begin_node_selection(id.as_str());
            (request, snapshot(&model))
        };
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
        }
        let Some(request) = request else {
            return;
        };
        run_node_selection(weak.clone(), model.clone(), request);
    });
}

fn wire_automatic_latency(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_secs(60), move || {
        if weak.upgrade().is_some_and(|ui| ui.window().is_visible()) {
            begin_latency_check(weak.clone(), model.clone());
        }
    });
    timer
}

fn begin_latency_check(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_latency_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    if let Some(request) = request {
        run_latency_check(weak, model, request);
    }
}

fn wire_event_filter(ui: &AppWindow, model: Arc<Mutex<DesktopViewModel>>) {
    let weak = ui.as_weak();
    ui.on_filter_events(move |filter| {
        let filter = filter.to_string();
        if let Some(ui) = weak.upgrade() {
            ui.set_event_filter(filter.clone().into());
        }
        let level = match filter.as_str() {
            "Ошибки" => Some("error".to_owned()),
            "Система" => Some("system".to_owned()),
            _ => None,
        };
        let events = model
            .lock()
            .expect("view model lock")
            .set_event_filter(level.as_deref());
        if let Some(ui) = weak.upgrade() {
            apply_events(&ui, events);
        }
    });
}

fn close_panel(
    weak: &slint::Weak<AppWindow>,
    model: &Arc<Mutex<DesktopViewModel>>,
    from_escape: bool,
) {
    let current = {
        let mut model = model.lock().expect("view model lock");
        if from_escape {
            model.close_panel_on_escape();
        } else {
            model.open_panel(Panel::None);
        }
        snapshot(&model)
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
}

fn run_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    let (client, mutation, current) = {
        let mut model = model.lock().expect("view model lock");
        let mutation = model.begin_refresh();
        (model.daemon_client(), mutation, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    run_status(weak, model, client, mutation, profile_preferences);
}

fn run_status(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    thread::spawn(move || {
        let result = client.status();
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_refresh(mutation, result);
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if refresh_catalog {
                load_profile_list(weak, model, profile_preferences, true);
            }
        });
    });
}

fn run_import(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    url: String,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    thread::spawn(move || {
        let result = client.import_subscription(&url);
        let import_succeeded = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_import(mutation, result);
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                if import_succeeded {
                    ui.set_import_success_serial(ui.get_import_success_serial().wrapping_add(1));
                }
            }
            if import_succeeded {
                advance_profile_reconciliation(weak, model, profile_preferences, true);
            }
        });
    });
}

fn run_connect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    queued: Vec<QueuedNodeSelection>,
) {
    thread::spawn(move || {
        let result = client.connect();
        let connect_succeeded = result.is_ok();
        let queued_results = if connect_succeeded {
            queued
                .iter()
                .map(|queued| {
                    client.select_node(&queued.group_id, queued.revision, &queued.node_id)
                })
                .collect()
        } else {
            Vec::new()
        };
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_connect(mutation, result);
            if connect_succeeded && !queued.is_empty() {
                model.finish_queued_node_selections(&queued, queued_results);
            }
            snapshot(&model)
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, false);
    });
}

fn run_profile_activation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
) {
    thread::spawn(move || {
        let result = client.activate_subscription(&profile_id);
        let current = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_activation(token, result);
            if succeeded {
                model.open_panel(Panel::None);
            }
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_target_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
) {
    thread::spawn(move || {
        let result = client.refresh_subscription_by_id(&profile_id);
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_profile_target_refresh(token, result);
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_source_edit(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
    url: String,
) {
    thread::spawn(move || {
        let result = client.replace_subscription_source(&profile_id, &url);
        let (current, succeeded) = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_source_edit(token, result);
            (snapshot(&model), succeeded)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                if succeeded {
                    ui.set_profile_edit_id("".into());
                    ui.set_profile_edit_draft("".into());
                }
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_profile_delete(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    client: Arc<dyn DaemonClient>,
    token: ProfileRequestToken,
    profile_id: String,
    focus_plan: DeleteFocusPlan,
) {
    thread::spawn(move || {
        let result = client.delete_subscription(&profile_id);
        let (current, succeeded) = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_profile_delete(token, result);
            (snapshot(&model), succeeded)
        };
        let completion = focus_plan.completion(succeeded);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                apply_delete_focus_completion(&ui, completion);
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn load_profile_list(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (token, client, current) = {
        let mut model = model.lock().expect("view model lock");
        if matches!(
            model.profile_presentation().pending_operation,
            Some(
                SubscriptionProfileOperation::Activate
                    | SubscriptionProfileOperation::Refresh
                    | SubscriptionProfileOperation::Edit
                    | SubscriptionProfileOperation::Delete
            )
        ) {
            return;
        }
        let token = model.begin_profile_list();
        (token, model.daemon_client(), snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(token) = token else {
        return;
    };
    thread::spawn(move || {
        let result = client.subscriptions();
        let succeeded = result.is_ok();
        let (accepted, current) = {
            let mut model = model.lock().expect("view model lock");
            let accepted = model.finish_profile_list(token, result);
            if accepted && succeeded {
                profile_preferences
                    .lock()
                    .expect("profile preference reconciler lock")
                    .reconcile_profile_list(&model);
            }
            (accepted, snapshot(&model))
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if accepted && succeeded {
                advance_profile_reconciliation(weak, model, profile_preferences, catalog_after);
            }
        });
    });
}

fn advance_profile_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (close_menu, reload_status, reload_list, reload_catalog) = {
        let mut model = model.lock().expect("view model lock");
        let close_menu = model.take_profile_close_intent();
        if close_menu {
            model.open_panel(Panel::None);
        }
        let reload_status = model.take_profile_status_reload_needed();
        let reload_list = !reload_status && model.take_profile_list_reload_needed();
        let reload_catalog =
            !reload_status && !reload_list && model.take_profile_catalog_reload_needed();
        (close_menu, reload_status, reload_list, reload_catalog)
    };
    if close_menu && let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &snapshot(&model.lock().expect("view model lock")));
    }
    if reload_status {
        run_profile_status_reconciliation(weak, model, profile_preferences, catalog_after);
    } else if reload_list {
        load_profile_list(weak, model, profile_preferences, catalog_after);
    } else if reload_catalog || catalog_after {
        load_profile_catalog(weak, model, profile_preferences);
    }
}

fn run_profile_status_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
    catalog_after: bool,
) {
    let (client, mutation, current) = {
        let mut model = model.lock().expect("view model lock");
        let mutation = model.begin_refresh();
        (model.daemon_client(), mutation, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    thread::spawn(move || {
        let result = client.status();
        let succeeded = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_refresh(mutation, result);
            snapshot(&model)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if succeeded {
                advance_profile_reconciliation(weak, model, profile_preferences, catalog_after);
            }
        });
    });
}

fn run_subscription_refresh(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    thread::spawn(move || {
        let result = client.refresh_subscription();
        let current = {
            let mut model = model.lock().expect("view model lock");
            let succeeded = model.finish_subscription_refresh(mutation, result);
            (snapshot(&model), succeeded)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current.0);
            }
            if current.1 && current.0.presentation.has_profile {
                load_catalog(weak.clone(), model.clone());
            }
            advance_profile_reconciliation(weak, model, profile_preferences, false);
        });
    });
}

fn run_disconnect(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.disconnect();
        let (reconciliation, current) = {
            let mut model = model.lock().expect("view model lock");
            let reconcile_status = model.finish_disconnect(mutation, result);
            let reconciliation = reconcile_status.then(|| model.begin_disconnect_reconciliation());
            (reconciliation, snapshot(&model))
        };
        apply_snapshot_from_worker(weak.clone(), current, model.clone(), false, false);
        if let Some(reconciliation) = reconciliation {
            run_disconnect_reconciliation(weak, model, client, reconciliation);
        }
    });
}

fn run_disconnect_reconciliation(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    client: Arc<dyn DaemonClient>,
    mutation: MutationToken,
) {
    thread::spawn(move || {
        let result = client.status();
        let refresh_catalog = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_disconnect_reconciliation(mutation, result)
                .then(|| snapshot(&model))
        };
        let Some(current) = current else {
            return;
        };
        apply_snapshot_from_worker(weak, current, model, refresh_catalog, false);
    });
}

fn apply_snapshot_from_worker(
    weak: slint::Weak<AppWindow>,
    current: UiSnapshot,
    model: Arc<Mutex<DesktopViewModel>>,
    refresh_catalog: bool,
    import_succeeded: bool,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            apply_snapshot(&ui, &current);
            if import_succeeded {
                ui.set_import_success_serial(ui.get_import_success_serial().wrapping_add(1));
            }
        }
        if refresh_catalog && current.presentation.has_profile {
            load_catalog(weak, model);
        }
    });
}

fn load_events(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let request = model
        .lock()
        .expect("view model lock")
        .begin_events_request();
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request
            .client
            .events(request.after, request.known_epoch.as_deref());
        let events = model
            .lock()
            .expect("view model lock")
            .finish_events(request.token, result);
        let Some(events) = events else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_events(&ui, events);
            }
        });
    });
}

fn load_diagnostics(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_diagnostics_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request.client.diagnostics();
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_diagnostics(request.token, result)
                .map(|events| (snapshot(&model), events))
        };
        let Some((current, events)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
                apply_events(&ui, events);
            }
        });
    });
}

fn load_catalog(weak: slint::Weak<AppWindow>, model: Arc<Mutex<DesktopViewModel>>) {
    load_catalog_impl(weak, model, None);
}

fn load_profile_catalog(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>,
) {
    load_catalog_impl(weak, model, Some(profile_preferences));
}

fn load_catalog_impl(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    profile_preferences: Option<Arc<Mutex<ProfilePreferenceReconciler>>>,
) {
    let (request, current) = {
        let mut model = model.lock().expect("view model lock");
        let request = model.begin_catalog_request();
        (request, snapshot(&model))
    };
    if let Some(ui) = weak.upgrade() {
        apply_snapshot(&ui, &current);
    }
    let Some(request) = request else {
        return;
    };
    thread::spawn(move || {
        let result = request.client.catalog();
        let catalog_succeeded = result.is_ok();
        let current = {
            let mut model = model.lock().expect("view model lock");
            model.finish_catalog(request.token, result).map(|_| {
                if let Some(preferences) = &profile_preferences {
                    preferences
                        .lock()
                        .expect("profile preference reconciler lock")
                        .apply_ready_hints_after_catalog(&mut model, catalog_succeeded);
                }
                snapshot(&model)
            })
        };
        let Some(current) = current else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if current.latency.queued {
                begin_latency_check(weak, model);
            }
        });
    });
}

fn run_latency_check(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    request: LatencyRequest,
) {
    thread::spawn(move || {
        let result = request
            .client
            .latencies(request.catalog_revision, &request.group_ids);
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_latencies(request.token, result)
                .map(|outcome| (snapshot(&model), outcome))
        };
        let Some((current, outcome)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if outcome.refresh_catalog {
                load_catalog(weak, model);
            } else if outcome.rerun {
                begin_latency_check(weak, model);
            }
        });
    });
}

fn run_node_selection(
    weak: slint::Weak<AppWindow>,
    model: Arc<Mutex<DesktopViewModel>>,
    request: NodeSelectionRequest,
) {
    thread::spawn(move || {
        let result =
            request
                .client
                .select_node(&request.group_id, request.revision, &request.node_id);
        let completion = {
            let mut model = model.lock().expect("view model lock");
            model
                .finish_node_selection(request.token, result)
                .map(|outcome| (snapshot(&model), outcome.refresh_catalog))
        };
        let Some((current, refresh_catalog)) = completion else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_snapshot(&ui, &current);
            }
            if refresh_catalog {
                load_catalog(weak, model);
            }
        });
    });
}

fn apply_events(ui: &AppWindow, events: Vec<view_model::PresentedEvent>) {
    let events = events
        .into_iter()
        .map(|event| UiEvent {
            timestamp: event.timestamp.into(),
            component: event.component.into(),
            level: event.level.into(),
            message: event.message.into(),
        })
        .collect::<Vec<_>>();
    ui.set_events(ModelRc::new(VecModel::from(events)));
}

fn snapshot(model: &DesktopViewModel) -> UiSnapshot {
    UiSnapshot {
        presentation: model.presentation(),
        panel: model.panel(),
        import_pending: model.import_pending(),
        import_error: model.import_error().unwrap_or_default().to_owned(),
        subscription: model.subscription_presentation(),
        profiles: model.profile_presentation(),
        catalog: model.catalog_presentation(),
        diagnostics: model.diagnostics_presentation(),
        latency: model.latency_presentation(),
    }
}

fn apply_snapshot(ui: &AppWindow, snapshot: &UiSnapshot) {
    let presentation = &snapshot.presentation;
    ui.set_status_eyebrow(SharedString::from(presentation.eyebrow));
    ui.set_headline(presentation.headline.clone().into());
    ui.set_supporting(presentation.supporting.clone().into());
    ui.set_node_name(presentation.node.clone().into());
    ui.set_has_profile(presentation.has_profile);
    ui.set_state_busy(presentation.is_busy);
    ui.set_update_runtime_safe(runtime_allows_update(presentation));
    ui.set_primary_label(SharedString::from(presentation.primary_label));
    ui.set_primary_enabled(presentation.primary_enabled);
    ui.set_status_label(SharedString::from(presentation.pill_label));
    ui.set_status_tone(
        match presentation.semantic_tone {
            SemanticTone::Neutral => "neutral",
            SemanticTone::Accent => "accent",
            SemanticTone::Success => "success",
            SemanticTone::Danger => "danger",
            SemanticTone::Warning => "warning",
        }
        .into(),
    );
    ui.set_import_pending(snapshot.import_pending);
    ui.set_import_error(snapshot.import_error.clone().into());
    ui.set_subscription_title(snapshot.subscription.display_name.clone().into());
    ui.set_subscription_usage(snapshot.subscription.usage.clone().into());
    ui.set_subscription_expiry(snapshot.subscription.expiry.clone().into());
    ui.set_subscription_updated(snapshot.subscription.updated.clone().into());
    ui.set_subscription_updated_stale(snapshot.subscription.updated_stale);
    ui.set_subscription_announcement_text(snapshot.subscription.announcement_text.clone().into());
    ui.set_subscription_announcement_tone(snapshot.subscription.announcement_tone.clone().into());
    let referenced_logo_paths = snapshot
        .subscription
        .service_logo_path
        .iter()
        .map(String::as_str)
        .chain(
            snapshot
                .profiles
                .profiles
                .iter()
                .filter_map(|profile| profile.service_logo_path.as_deref()),
        )
        .collect::<Vec<_>>();
    let (service_logo, subscription_profiles) = logo_cache::with_logo_cache(|cache| {
        cache.retain_paths(referenced_logo_paths.iter().copied());
        let service_logo = snapshot
            .subscription
            .service_logo_path
            .as_deref()
            .and_then(|path| cache.image_for_path(path));
        let profiles = snapshot
            .profiles
            .profiles
            .iter()
            .map(|profile| {
                let logo = profile
                    .service_logo_path
                    .as_deref()
                    .and_then(|path| cache.image_for_path(path));
                UiSubscriptionProfile {
                    id: profile.id.clone().into(),
                    title: profile.title.clone().into(),
                    usage: profile.usage.clone().into(),
                    expiry: profile.expiry.clone().into(),
                    active: profile.active,
                    available: profile.available,
                    has_logo: logo.is_some(),
                    logo: logo.unwrap_or_default(),
                    fallback: profile
                        .title
                        .chars()
                        .next()
                        .map(|letter| letter.to_uppercase().to_string())
                        .unwrap_or_else(|| "M".into())
                        .into(),
                }
            })
            .collect::<Vec<_>>();
        (service_logo, profiles)
    });
    ui.set_has_service_logo(service_logo.is_some());
    ui.set_service_logo(service_logo.unwrap_or_default());
    ui.set_subscription_refresh_available(snapshot.subscription.refresh_available);
    ui.set_subscription_refresh_enabled(presentation.has_profile && !presentation.is_busy);
    ui.set_subscription_management_enabled(
        presentation.has_profile && !presentation.is_connected && !presentation.is_busy,
    );
    ui.set_subscription_refresh_pending(snapshot.subscription.refresh_pending);
    ui.set_subscription_refresh_error(
        snapshot
            .subscription
            .error
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_subscription_profiles(ModelRc::new(VecModel::from(subscription_profiles)));
    ui.set_profile_menu_loading(snapshot.profiles.loading);
    ui.set_profile_menu_error(snapshot.profiles.error.clone().unwrap_or_default().into());
    ui.set_profile_pending_operation(
        match snapshot.profiles.pending_operation {
            Some(SubscriptionProfileOperation::List) => "list",
            Some(SubscriptionProfileOperation::Activate) => "activate",
            Some(SubscriptionProfileOperation::Refresh) => "refresh",
            Some(SubscriptionProfileOperation::Edit) => "edit",
            Some(SubscriptionProfileOperation::Delete) => "delete",
            None => "none",
        }
        .into(),
    );
    ui.set_profile_pending_id(
        snapshot
            .profiles
            .pending_id
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_diagnostics_xray_value(snapshot.diagnostics.xray.value.clone().into());
    ui.set_diagnostics_xray_tone(snapshot.diagnostics.xray.tone.clone().into());
    ui.set_diagnostics_mihomo_value(snapshot.diagnostics.mihomo.value.clone().into());
    ui.set_diagnostics_mihomo_tone(snapshot.diagnostics.mihomo.tone.clone().into());
    ui.set_diagnostics_tun_value(snapshot.diagnostics.tun.value.clone().into());
    ui.set_diagnostics_tun_tone(snapshot.diagnostics.tun.tone.clone().into());
    ui.set_diagnostics_loading(snapshot.diagnostics.loading);
    ui.set_latency_check_loading(snapshot.latency.loading);
    ui.set_latency_check_queued(snapshot.latency.queued);
    ui.set_latency_check_error(snapshot.latency.error.clone().unwrap_or_default().into());
    ui.set_diagnostic_mappings(ModelRc::new(VecModel::from(
        snapshot
            .diagnostics
            .mappings
            .iter()
            .map(|mapping| UiProxyMapping {
                proxy_name: mapping.proxy_name.clone().into(),
                address: mapping.address.clone().into(),
                xray_label: mapping.xray_label.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
    apply_catalog(ui, &snapshot.catalog);
    ui.set_active_panel(
        match snapshot.panel {
            Panel::None => "none",
            Panel::Subscription => "subscription",
            Panel::Routes => "routes",
            Panel::Events => "events",
        }
        .into(),
    );
}

fn apply_catalog(ui: &AppWindow, catalog: &CatalogPresentation) {
    let groups = catalog
        .groups
        .iter()
        .map(|group| {
            let display = derive_catalog_display(&group.label, CatalogDisplayKind::Group);
            UiCatalogGroup {
                id: group.id.clone().into(),
                icon: display.icon.into(),
                label: display.label.into(),
                selected: group.selected,
            }
        })
        .collect::<Vec<_>>();
    let nodes = catalog
        .nodes
        .iter()
        .map(|node| {
            let display = derive_catalog_display(&node.label, CatalogDisplayKind::Node);
            UiCatalogNode {
                id: node.id.clone().into(),
                icon: display.icon.into(),
                label: display.label.into(),
                selected: node.selected,
                latency_text: node.latency_text.clone().into(),
                latency_tone: node.latency_tone.clone().into(),
            }
        })
        .collect::<Vec<_>>();
    ui.set_catalog_groups(ModelRc::new(VecModel::from(groups)));
    ui.set_catalog_nodes(ModelRc::new(VecModel::from(nodes)));
    ui.set_catalog_loading(catalog.loading);
    ui.set_catalog_selection_enabled(catalog.selection_enabled);
    ui.set_catalog_selection_pending(catalog.selection_pending);
    ui.set_catalog_selection_queued(catalog.selection_queued);
    ui.set_catalog_error(catalog.error.clone().unwrap_or_default().into());
}

#[cfg(test)]
mod window_tests {
    use super::{
        DeleteFocusState, DeleteFocusTarget, LaunchArguments, LaunchMode, PREFERENCE_RETRY_DELAYS,
        PREFERENCE_SAVE_ERROR_RU, PersistenceTracker, PreferenceWriter,
        ProfilePreferenceReconciler, WindowObservation, WriterCommand, WriterResult,
        activation_for_launch_arguments, ambient_motion_allowed, delete_focus_plan,
        drain_latest_write, initial_window_visible, next_maximized, parse_launch_arguments,
        preference_error_for_result, reduced_motion_from_client_area_animation,
        retry_delay_after_failure, route_hints_apply_to, runtime_allows_update, scale_factor_milli,
        shell_active_after_native_minimize_change, visible_page_from_name, visible_page_name,
    };
    use crate::daemon::test_support::MockDaemonClient;
    use crate::daemon::{
        DaemonCatalog, DaemonCatalogGroup, DaemonCatalogNode, DaemonError, DaemonStatus,
    };
    use crate::preferences::{AppPreferences, PreferenceStore, VisiblePage, WindowBounds};
    use crate::single_instance::Activation;
    use crate::view_model::{DesktopViewModel, UiState};
    use crate::windows_shell::{
        CloseDisposition, MinimizeDisposition, close_disposition, minimize_disposition,
        parse_resize_edge,
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn maximize_toggle_inverts_current_window_state() {
        assert!(next_maximized(false));
        assert!(!next_maximized(true));
    }

    #[test]
    fn windows_client_animation_preference_has_a_safe_testable_mapping() {
        assert!(!reduced_motion_from_client_area_animation(Some(true)));
        assert!(reduced_motion_from_client_area_animation(Some(false)));
        assert!(!reduced_motion_from_client_area_animation(None));

        let source = include_str!("main.rs");
        assert!(source.contains("SPI_GETCLIENTAREAANIMATION"));
        assert!(source.contains("client_area_animation_enabled()"));
        assert!(source.contains("ui.set_reduced_motion("));
    }

    #[test]
    fn ambient_motion_gate_stops_on_every_inactive_path() {
        assert!(ambient_motion_allowed(true, true, false, false, true));
        assert!(!ambient_motion_allowed(false, true, false, false, true));
        assert!(!ambient_motion_allowed(true, false, false, false, true));
        assert!(!ambient_motion_allowed(true, true, true, false, true));
        assert!(!ambient_motion_allowed(true, true, false, true, true));
        assert!(!ambient_motion_allowed(true, true, false, false, false));
    }

    #[test]
    fn branded_tray_popup_source_contract() {
        let app = include_str!("../ui/app.slint");
        let popup = include_str!("../ui/tray_popup.slint");
        let rust = include_str!("main.rs");

        for required in [
            "export { TrayPopup, TrayMenuRow } from \"tray_popup.slint\";",
            "export component TrayPopup inherits Window",
            "no-frame: true;",
            "always-on-top: true;",
            "import \"../assets/fonts/Twemoji.Mozilla.ttf\";",
            "icon: string,",
            "detail: string,",
            "expanded: bool,",
            "node-count: int,",
            "callback toggle-group(string);",
            "component TrayConnectAction inherits Rectangle",
            "connect-mesh-field := Rectangle",
            "icon-kind: \"window\"",
            "detail: group.detail;",
            "expanded: group.expanded;",
            "visible: node.group-id == group.id;",
            "TrayInteraction::ShowMenu",
            "TrayAccordionState",
            "place_tray_popup(",
            ".take(12)",
            ".take(64)",
        ] {
            assert!(
                app.contains(required) || popup.contains(required) || rust.contains(required),
                "missing branded tray popup contract: {required}"
            );
        }
        assert!(!popup.contains("text: \"УЗЕЛ\";"));
        assert!(!popup.contains("expanded-group-id"));
        assert!(!popup.contains("marker: \"↗\""));
    }

    #[test]
    fn tray_popup_hidden_nodes_do_not_accumulate_layout_spacing() {
        let popup = include_str!("../ui/tray_popup.slint");
        let expanded_nodes = popup
            .split("if group.expanded: VerticalLayout")
            .nth(1)
            .and_then(|source| source.split("if root.group-rows.length == 0").next())
            .expect("expanded tray node layout");

        assert!(expanded_nodes.contains("spacing: 0px;"));
        assert!(expanded_nodes.contains("height: node.group-id == group.id ? 42px : 0px;"));
        assert!(!expanded_nodes.contains("height: self.visible ? 40px : 0px;"));
    }

    #[test]
    fn tray_popup_uses_an_opaque_fallback_and_native_windows_rounding() {
        let popup = include_str!("../ui/tray_popup.slint");
        let rust = include_str!("main.rs");
        let shell = include_str!("windows_shell.rs");
        let window_header = popup
            .split("export component TrayPopup inherits Window")
            .nth(1)
            .and_then(|source| source.split("in property <string> status-text").next())
            .expect("tray popup window header");

        assert!(window_header.contains("background: #15181d;"));
        assert!(!window_header.contains("background: transparent;"));
        assert!(rust.contains("apply_tray_popup_native_rounding(window)"));
        assert!(shell.contains("DwmSetWindowAttribute"));
        assert!(shell.contains("DWMWA_WINDOW_CORNER_PREFERENCE"));
        assert!(shell.contains("DWMWCP_ROUND"));
    }

    #[test]
    fn tray_popup_lifecycle_keeps_route_accordion_open_but_closes_terminal_actions() {
        let popup = include_str!("../ui/tray_popup.slint");
        let rust = include_str!("main.rs");

        assert!(popup.contains("event.text == Key.Escape"));
        assert!(popup.contains("root.dismiss();"));
        let dispatch = rust
            .split("fn dispatch_tray_popup_command(")
            .nth(1)
            .and_then(|source| source.split("fn wire_tray_popup_actions(").next())
            .expect("tray popup command dispatcher");
        assert!(
            dispatch.find("hide_tray_popup").unwrap()
                < dispatch.find("handle_tray_command").unwrap()
        );
        let selection_dispatch = rust
            .split("fn dispatch_tray_popup_selection(")
            .nth(1)
            .and_then(|source| source.split("fn wire_tray_popup_actions(").next())
            .expect("tray popup selection dispatcher");
        assert!(!selection_dispatch.contains("hide_tray_popup"));
        assert!(selection_dispatch.contains("handle_tray_command"));
        assert!(selection_dispatch.contains("apply_tray_popup_model"));
        let main_entry = rust
            .split("fn main()")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments(").next())
            .expect("desktop main entrypoint");
        let compact_main = main_entry.split_whitespace().collect::<String>();
        assert!(compact_main.contains(
            "wire_tray_popup_focus_loss(&tray_popup,tray_popup_open.clone(),model.clone(),tray_accordion.clone(),)"
        ));
        assert!(rust.contains("if open.get() {\n        hide_tray_popup"));
        assert!(rust.contains("with_winit_window(|window| window.has_focus())"));
        assert!(rust.contains("let _ = tray_popup.hide();"));
    }

    #[test]
    fn tray_route_group_uses_the_whole_row_as_its_accordion_trigger() {
        let popup = include_str!("../ui/tray_popup.slint");
        let route_group = popup
            .split("component RouteGroupAction inherits Rectangle")
            .nth(1)
            .and_then(|source| {
                source
                    .split("export component TrayPopup inherits Window")
                    .next()
            })
            .expect("route group component source");

        for required in [
            "route-group-focus := FocusScope",
            "width: parent.width;",
            "height: parent.height;",
            "route-group-touch := TouchArea",
            "clicked => { root.activated(); }",
        ] {
            assert!(
                route_group.contains(required),
                "route-group row must expose a full-surface accordion trigger: {required}"
            );
        }
    }

    #[test]
    fn native_minimize_and_restore_drive_shell_activity_without_tray_wakeup() {
        assert!(!shell_active_after_native_minimize_change(true));
        assert!(shell_active_after_native_minimize_change(false));

        let source = include_str!("../ui/app.slint");
        let handler = source
            .split("changed minimized => {")
            .nth(1)
            .and_then(|source| source.split('}').next())
            .expect("native minimized change handler");
        assert!(handler.contains("root.shell-active = !root.minimized;"));
        assert!(!handler.contains("if (!root.minimized)"));
    }

    #[test]
    fn ambient_preference_and_shell_visibility_are_wired_to_native_lifecycle() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        tracker.set_ambient_background(false);
        assert!(!tracker.latest_desired().ambient_background);

        let source = include_str!("main.rs");
        assert!(
            source.contains(
                "ui.set_ambient_background_enabled(loaded_preferences.ambient_background)"
            )
        );
        assert!(
            source.contains("tracker.set_ambient_background(ui.get_ambient_background_enabled())")
        );
        assert!(source.contains("ui.set_shell_active(starts_visible)"));
        assert!(source.matches("ui.set_shell_active(false)").count() >= 2);
        assert!(source.matches("ui.set_shell_active(true)").count() >= 2);
    }

    #[test]
    fn automatic_subscription_refresh_and_home_timestamp_are_wired() {
        let rust = include_str!("main.rs");
        let ui = include_str!("../ui/app.slint");

        for required in [
            "fn wire_automatic_subscription_refresh(",
            "profile_preferences: Arc<Mutex<ProfilePreferenceReconciler>>",
            "automatic_subscription_refresh_due_at(now_unix)",
            "Duration::from_secs(60)",
            "set_subscription_updated",
            "set_subscription_updated_stale",
            "advance_profile_reconciliation(weak, model, profile_preferences, false)",
        ] {
            assert!(
                rust.contains(required),
                "missing auto-refresh contract: {required}"
            );
        }
        for required in [
            "in property <string> subscription-updated",
            "in property <bool> subscription-updated-stale",
            "updated-meta := Rectangle",
            "text: root.subscription-updated",
            "subscription-refresh-action := FocusScope",
            "text: root.subscription-refresh-pending ? \"Обновляем…\" : \"Обновить\"",
            "root.refresh-subscription();",
            "accessible-description: root.subscription-title + \". Обновлено \" + root.subscription-updated",
            "announcement-strip := Rectangle",
            "width: Math.min(540px, parent.width - 24px)",
        ] {
            assert!(
                ui.contains(required),
                "missing updated-at UI contract: {required}"
            );
        }
        let updated_meta = ui
            .split("updated-meta := Rectangle")
            .nth(1)
            .and_then(|source| source.split("shelf-chevron := Rectangle").next())
            .expect("updated-at metadata source");
        assert!(!updated_meta.contains("text: \"Обновлено\""));
        assert!(!updated_meta.contains("refresh.svg"));
        assert!(!updated_meta.contains("Theme.warning"));
        assert!(!updated_meta.contains("subscription-updated-stale ?"));
        assert!(rust.contains("presentation.has_profile && !presentation.is_busy"));
        assert!(rust.contains("ui.set_subscription_management_enabled("));
    }

    #[test]
    fn route_preferences_flow_from_startup_model_to_atomic_persistence() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let selections =
            std::collections::BTreeMap::from([("group-a".to_owned(), "node-b".to_owned())]);
        tracker.set_route_preferences(
            "profile-a".to_owned(),
            Some("group-a".to_owned()),
            selections.clone(),
        );
        assert_eq!(tracker.latest_desired().active_profile_hint, None);
        tracker.set_active_profile_hint(Some("profile-a".to_owned()));
        let saved = tracker.snapshot_for_flush(None);
        assert_eq!(saved.active_profile_hint.as_deref(), Some("profile-a"));
        assert_eq!(
            saved
                .last_group_by_profile
                .get("profile-a")
                .map(String::as_str),
            Some("group-a")
        );
        assert_eq!(
            saved.selections_by_profile.get("profile-a"),
            Some(&selections)
        );

        let source = include_str!("main.rs");
        assert!(source.contains("model.restore_route_preferences("));
        assert!(source.contains("set_active_profile_hint(active_id.clone())"));
        assert!(source.contains("tracker.set_route_preferences("));
    }

    #[test]
    fn authoritative_profile_hint_is_updated_independently_of_route_preferences() {
        let mut tracker = PersistenceTracker::new(AppPreferences {
            active_profile_hint: Some("old-profile".into()),
            ..AppPreferences::default()
        });
        tracker.set_route_preferences("new-profile".into(), None, Default::default());
        assert_eq!(
            tracker.latest_desired().active_profile_hint.as_deref(),
            Some("old-profile")
        );
        tracker.set_active_profile_hint(Some("daemon-profile".into()));
        assert_eq!(
            tracker.latest_desired().active_profile_hint.as_deref(),
            Some("daemon-profile")
        );
        tracker.set_active_profile_hint(None);
        assert_eq!(tracker.latest_desired().active_profile_hint, None);
    }

    #[test]
    fn route_hints_are_mutable_across_a_b_a_and_apply_only_while_ready() {
        let tracker = Arc::new(Mutex::new(PersistenceTracker::new(
            AppPreferences::default(),
        )));
        let mut reconciler = ProfilePreferenceReconciler::new(AppPreferences::default(), tracker);
        reconciler.record_hint(
            "profile-a".into(),
            Some("group-a".into()),
            std::collections::BTreeMap::from([("group-a".into(), "node-a".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a")
        );
        reconciler.record_hint(
            "profile-b".into(),
            Some("group-b".into()),
            std::collections::BTreeMap::from([("group-b".into(), "node-b".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-b".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-b")
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a")
        );
        assert_eq!(
            reconciler
                .hints
                .last_group_by_profile
                .get("profile-a")
                .map(String::as_str),
            Some("group-a")
        );
        assert_eq!(
            reconciler
                .hints
                .last_group_by_profile
                .get("profile-b")
                .map(String::as_str),
            Some("group-b")
        );

        let ready = UiState::Ready {
            profile: "A".into(),
            node: "auto".into(),
        }
        .presentation();
        let connected = UiState::Connected {
            profile: "A".into(),
            node: "daemon".into(),
        }
        .presentation();
        assert!(route_hints_apply_to(&ready));
        assert!(!route_hints_apply_to(&connected));
    }

    #[test]
    fn failed_catalog_keeps_pending_profile_hints_for_a_successful_retry() {
        let tracker = Arc::new(Mutex::new(PersistenceTracker::new(
            AppPreferences::default(),
        )));
        let mut reconciler = ProfilePreferenceReconciler::new(AppPreferences::default(), tracker);
        reconciler.record_hint(
            "profile-a".into(),
            Some("group-a".into()),
            std::collections::BTreeMap::from([("group-a".into(), "node-a-saved".into())]),
        );
        reconciler.set_authoritative_profile(Some("profile-a".into()));

        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let status = model.begin_refresh();
        model.finish_refresh(
            status,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Profile A".into()),
                ..DaemonStatus::default()
            }),
        );

        let failed = model
            .begin_catalog_request()
            .expect("failed catalog request");
        model.finish_catalog(failed.token, Err(DaemonError::new("temporary failure")));
        reconciler.apply_ready_hints_after_catalog(&mut model, false);
        assert_eq!(
            reconciler.pending_ready_profile.as_deref(),
            Some("profile-a"),
            "a transient catalog error must not consume the profile hint"
        );

        let retry = model.begin_catalog_request().expect("catalog retry");
        model.finish_catalog(
            retry.token,
            Ok(DaemonCatalog {
                revision: 7,
                groups: vec![DaemonCatalogGroup {
                    id: "group-a".into(),
                    label: "Primary".into(),
                    selected: true,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "node-a-default".into(),
                            label: "Default".into(),
                            selected: true,
                            delay_ms: None,
                        },
                        DaemonCatalogNode {
                            id: "node-a-saved".into(),
                            label: "Saved".into(),
                            selected: false,
                            delay_ms: None,
                        },
                    ],
                }],
            }),
        );
        reconciler.apply_ready_hints_after_catalog(&mut model, true);

        assert_eq!(reconciler.pending_ready_profile, None);
        let (_, selected_group, selections) = model
            .route_preferences()
            .expect("restored route preferences");
        assert_eq!(selected_group.as_deref(), Some("group-a"));
        assert_eq!(
            selections.get("group-a").map(String::as_str),
            Some("node-a-saved")
        );
    }

    #[test]
    fn settings_inline_profile_tasks_consume_escape_before_panel_close() {
        let source = include_str!("../ui/app.slint");
        let escape_handler = source
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|source| source.split("VerticalLayout {").next())
            .expect("visual frame Escape handler");
        assert!(escape_handler.contains("if (root.profile-delete-confirm-id != \"\")"));
        assert!(escape_handler.contains("root.profile-delete-confirm-id = \"\";"));
        assert!(escape_handler.contains("else if (root.profile-edit-id != \"\")"));
        assert!(escape_handler.contains("root.profile-edit-id = \"\";"));
        assert!(escape_handler.contains("root.profile-edit-draft = \"\";"));
        assert!(escape_handler.contains("else { root.escape-pressed(); }"));
    }

    #[test]
    fn delete_focus_plan_waits_for_completion_and_covers_every_outcome() {
        assert_eq!(
            delete_focus_plan(&["profile-a", "profile-b"], "profile-a", false),
            DeleteFocusState::Immediate(DeleteFocusTarget::DeleteControl("profile-a".into())),
            "preflight rejection closes confirmation and restores its delete control"
        );

        let DeleteFocusState::AwaitingCompletion(middle) =
            delete_focus_plan(&["profile-a", "profile-b", "profile-c"], "profile-b", true)
        else {
            panic!("accepted middle-row deletion must wait for completion");
        };
        assert_eq!(
            middle.completion(true),
            DeleteFocusTarget::ManagementProfile("profile-c".into())
        );
        assert_eq!(
            middle.completion(false),
            DeleteFocusTarget::DeleteControl("profile-b".into())
        );

        let DeleteFocusState::AwaitingCompletion(last) =
            delete_focus_plan(&["profile-a", "profile-b"], "profile-b", true)
        else {
            panic!("accepted last-row deletion must wait for completion");
        };
        assert_eq!(
            last.completion(true),
            DeleteFocusTarget::ManagementProfile("profile-a".into())
        );

        let DeleteFocusState::AwaitingCompletion(single) =
            delete_focus_plan(&["profile-a"], "profile-a", true)
        else {
            panic!("accepted single-row deletion must wait for completion");
        };
        assert_eq!(single.completion(true), DeleteFocusTarget::AddSubscription);
    }

    #[test]
    fn profile_delete_ui_defers_management_focus_until_completion() {
        let ui = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");

        for required in [
            "in-out property <string> profile-management-focus-id;",
            "in-out property <int> profile-management-focus-serial;",
            "in-out property <string> profile-delete-control-focus-id;",
            "in-out property <int> profile-delete-control-focus-serial;",
            "root.profile-management-focus-id == profile.id",
            "root.profile-delete-control-focus-id == profile.id",
            "focus-epoch: root.profile-management-focus-id == \"\" ? root.profile-management-focus-serial : 0;",
        ] {
            assert!(
                ui.contains(required),
                "missing delete focus contract: {required}"
            );
        }
        let delete_worker = main
            .split("fn run_profile_delete(")
            .nth(1)
            .and_then(|source| source.split("fn load_profile_list(").next())
            .expect("delete worker");
        assert!(delete_worker.contains("let completion = focus_plan.completion(succeeded);"));
        assert!(
            delete_worker
                .find("apply_snapshot(&ui, &current);")
                .expect("authoritative snapshot application")
                < delete_worker
                    .find("apply_delete_focus_completion(&ui, completion);")
                    .expect("focus completion after snapshot")
        );
        let delete_wiring = main
            .split("ui.on_delete_profile(move |profile_id| {")
            .nth(1)
            .and_then(|source| source.split("fn wire_primary_action(").next())
            .expect("delete callback wiring");
        assert!(delete_wiring.contains("DeleteFocusState::Immediate(completion)"));
        assert!(
            delete_wiring
                .find("apply_snapshot(&ui, &current);")
                .expect("preflight rejection snapshot")
                < delete_wiring
                    .find("apply_delete_focus_completion(&ui, completion.clone());")
                    .expect("preflight rejection focus restoration")
        );
        assert!(main.contains("ui.set_profile_delete_confirm_id(\"\".into());"));
        assert!(ui.contains("root.profile-delete-control-focus-id = profile.id;"));
        assert!(ui.contains("root.profile-delete-control-focus-serial += 1;"));
        assert!(
            !ui.contains("root.profile-delete-confirm-id = \"\"; root.delete-profile(profile.id);")
        );
    }

    #[test]
    fn pointer_closed_selector_does_not_restore_keyboard_focus_ring() {
        let ui = include_str!("../ui/app.slint");
        let panel_change = ui
            .split("changed active-panel => {")
            .nth(1)
            .and_then(|source| source.split("visual-frame := FocusScope {").next())
            .expect("active panel change handler");

        assert!(
            panel_change
                .contains("if (root.profile-menu-was-open && root.profile-menu-keyboard-open)")
        );
        assert!(!panel_change.contains(
            "if (root.profile-menu-was-open) {\n                root.profile-menu-was-open = false;\n                root.profile-shelf-focus-serial += 1;"
        ));
    }

    #[test]
    fn compact_profile_switcher_source_contract() {
        let ui = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");
        let fixture = include_str!("../../../scripts/preview-fixture.ps1");
        let capture = include_str!("../../../scripts/capture-preview.ps1");

        for required in [
            "export struct UiSubscriptionProfile",
            "root.active-panel == \"subscription\"",
            "callback activate-profile(string);",
            "callback refresh-profile(string);",
            "callback replace-profile-source(string, string);",
            "callback delete-profile(string);",
            "callback add-subscription();",
            "callback retry-profile-menu();",
            "profile-menu := Rectangle",
            "profile-scroll := ScrollView",
            "Подписок пока нет",
            "Удалить подписку?",
            "До 16 подписок · активна только одна",
            "Изменить URL",
            "root.settings-section == \"updates\"",
            "profile-menu-keyboard-open",
            "parent.has-focus && root.profile-menu-keyboard-open",
            "accessible-label: \"Выбрать профиль подписки\"",
            "Загружаем профили…",
            "Повторить загрузку профилей",
            "@image-url(\"../assets/refresh.svg\")",
            "@image-url(\"../assets/trash.svg\")",
        ] {
            assert!(
                ui.contains(required),
                "missing profile UI contract: {required}"
            );
        }
        assert!(!ui.contains("label: \"Профили\";"));
        assert!(!ui.contains("drop-shadow-blur: 18px"));
        assert!(!ui.contains("text: \"Обн.\""));
        assert!(!ui.contains("text: \"×\""));
        assert!(ui.contains("width: Math.min(320px, parent.width - 40px);"));
        assert!(ui.contains("Math.min(288px, 16px + root.subscription-profiles.length * 54px)"));
        assert!(!ui.contains("text: \"Профили\""));
        assert!(main.contains("model.finish_profile_list(token, result)"));
        assert!(main.contains("model.finish_profile_activation(token, result)"));
        assert!(main.contains("model.finish_profile_target_refresh(token, result)"));
        assert!(main.contains("model.finish_profile_delete(token, result)"));
        assert!(main.contains("slint::invoke_from_event_loop"));
        assert!(main.contains("set_active_profile_hint(active_id.clone())"));
        assert!(main.contains("capture_current(&model)"));
        assert!(main.contains("apply_ready_hints_after_catalog(&mut model)"));
        for route in [
            "GET\" -and $path -eq \"/v1/subscriptions",
            "PUT\" -and $path -match '^/v1/subscriptions/([^/]+)/activate$'",
            "POST\" -and $path -match '^/v1/subscriptions/([^/]+)/refresh$'",
            "PUT\" -and $path -match '^/v1/subscriptions/([^/]+)/source$'",
            "DELETE\" -and $path -match '^/v1/subscriptions/([^/]+)$'",
        ] {
            assert!(fixture.contains(route), "missing fixture route: {route}");
        }
        assert!(capture.contains("Click-Preview $handle 276 203"));
        assert!(capture.contains("if ($EditProfile)"));
        assert!(capture.contains("($Width - 92) 224"));
        assert!(!capture.contains("if ($ExpandProfile)"));
        assert!(capture.contains("profile flow replaced the navigation rail"));
    }

    #[test]
    fn final_event_loop_exit_flushes_immediate_ambient_changes() {
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        tracker.set_ambient_background(false);
        let final_snapshot = tracker.snapshot_for_flush(None);
        assert!(!final_snapshot.ambient_background);

        let source = include_str!("main.rs");
        let shutdown = source
            .split("let result = slint::run_event_loop();")
            .nth(1)
            .and_then(|source| source.split("fn activation_for_launch_arguments").next())
            .expect("common post-event-loop shutdown");
        assert!(
            shutdown.contains(
                "flush_preferences(&ui, &model, &preference_writer, &preference_tracker);"
            )
        );
        assert!(shutdown.find("flush_preferences").unwrap() < shutdown.find("ui.hide()").unwrap());
        assert!(!shutdown.contains(".stop();"));

        let updater_exit = source
            .split("fn wire_updater(")
            .nth(1)
            .and_then(|source| source.split("fn run_update_check").next())
            .expect("updater wiring");
        assert!(updater_exit.contains("slint::quit_event_loop()"));
    }

    #[test]
    fn final_flush_can_replace_an_earlier_tray_flush_after_writer_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        let tray_snapshot = AppPreferences::default();
        assert!(writer.stop_and_flush(&tray_snapshot));

        let final_snapshot = AppPreferences {
            ambient_background: false,
            visible_page: VisiblePage::Settings,
            ..tray_snapshot
        };
        assert!(writer.stop_and_flush(&final_snapshot));
        assert_eq!(store.load().unwrap(), final_snapshot);
    }

    #[test]
    fn winit_scale_factor_is_bounded_before_entering_placement_model() {
        assert_eq!(scale_factor_milli(1.0), Some(1_000));
        assert_eq!(scale_factor_milli(2.0), Some(2_000));
        assert_eq!(scale_factor_milli(f64::NAN), None);
        assert_eq!(scale_factor_milli(f64::INFINITY), None);
        assert_eq!(scale_factor_milli(0.49), None);
        assert_eq!(scale_factor_milli(8.01), None);
    }

    #[test]
    fn close_hides_only_when_a_tray_is_available_outside_smoke_mode() {
        assert_eq!(close_disposition(true, false), CloseDisposition::HideToTray);
        assert_eq!(close_disposition(false, false), CloseDisposition::Exit);
        assert_eq!(close_disposition(true, true), CloseDisposition::Exit);
        assert_eq!(minimize_disposition(true), MinimizeDisposition::HideToTray);
        assert_eq!(
            minimize_disposition(false),
            MinimizeDisposition::MinimizeToTaskbar
        );
        let close_handler = include_str!("main.rs")
            .split("fn close_window(")
            .nth(1)
            .and_then(|source| source.split("fn tray_menu_model").next())
            .expect("close handler");
        assert!(!close_handler.contains("MinimizeToTaskbar"));
    }

    #[test]
    fn tray_hide_keeps_slint_window_registered_for_tray_events() {
        let source = include_str!("main.rs");
        let helper = source
            .split("fn hide_window_to_tray(")
            .nth(1)
            .and_then(|source| source.split("fn tray_menu_model").next())
            .expect("tray hide helper");
        assert!(helper.contains("with_winit_window"));
        assert!(helper.contains("window.set_visible(false)"));
        assert!(!helper.contains("ui.hide()"));
    }

    #[test]
    fn page_names_round_trip_and_unknown_names_are_rejected() {
        for page in [
            VisiblePage::Home,
            VisiblePage::Status,
            VisiblePage::Subscriptions,
            VisiblePage::Settings,
        ] {
            assert_eq!(visible_page_from_name(visible_page_name(&page)), Some(page));
        }
        assert_eq!(visible_page_from_name("routes"), None);
    }

    #[test]
    fn subscriptions_are_a_persisted_top_level_page() {
        let ui = include_str!("../ui/app.slint");
        let rust = include_str!("main.rs");
        let preferences = include_str!("preferences.rs");

        for required in [
            "label: \"Подписки\"; active: root.visible-page == \"subscriptions\"",
            "if root.visible-page == \"subscriptions\"",
            "VisiblePage::Subscriptions => \"subscriptions\"",
            "\"subscriptions\" => Some(VisiblePage::Subscriptions)",
            "Subscriptions,",
            "if root.visible-page == \"settings\" && root.settings-section == \"general\"",
            "if root.visible-page == \"settings\" && root.settings-section == \"updates\"",
        ] {
            assert!(
                ui.contains(required) || rust.contains(required) || preferences.contains(required),
                "missing top-level subscriptions contract: {required}"
            );
        }
        assert!(!ui.contains("SettingsNavItem { width: parent.width; label: \"Подписки\""));
        assert!(!rust.contains("set_settings_section(\"subscriptions\""));
    }

    #[test]
    fn subscription_rows_expose_metadata_and_actions_without_generic_expansion() {
        let ui = include_str!("../ui/app.slint");

        assert!(ui.contains("in-out property <string> profile-edit-id;"));
        assert!(ui.contains("in-out property <string> profile-edit-draft;"));
        assert!(ui.contains("root.replace-profile-source(profile.id, root.profile-edit-draft);"));
        assert!(ui.contains("profile.usage + \" · \" + profile.expiry"));
        assert!(ui.contains("accessible-label: \"Изменить URL \" + profile.title"));
        assert!(ui.contains("@image-url(\"../assets/edit.svg\")"));
        let edit_control = ui
            .split("edit-profile-focus := FocusScope")
            .nth(1)
            .and_then(|source| source.split("delete-profile-focus := FocusScope").next())
            .expect("profile edit control source");
        assert!(!edit_control.contains("@image-url(\"../assets/link.svg\")"));
        assert!(!ui.contains("expanded-profile-id"));
        assert!(!ui.contains("Локальный источник"));
    }

    #[test]
    fn persistence_waits_for_two_stable_ticks_and_skips_unchanged_state() {
        let initial = AppPreferences::default();
        let mut tracker = PersistenceTracker::new(initial.clone());
        let changed = WindowObservation {
            restored_bounds: Some(WindowBounds {
                x: 10,
                y: 20,
                width: 900,
                height: 700,
            }),
            maximized: false,
            visible_page: VisiblePage::Settings,
        };

        assert_eq!(tracker.observe(Some(changed.clone())), None);
        let persisted = tracker
            .observe(Some(changed.clone()))
            .expect("stable change");
        assert_eq!(persisted.restored_bounds, changed.restored_bounds);
        assert_eq!(persisted.visible_page, VisiblePage::Settings);
        tracker.mark_persisted(persisted);
        assert_eq!(tracker.observe(Some(changed.clone())), None);
        assert_eq!(tracker.observe(None), None);
        assert_eq!(tracker.observe(Some(changed)), None);
    }

    #[test]
    fn persistence_queue_coalesces_latest_and_suppresses_failed_snapshot_repeats() {
        let mut first = AppPreferences {
            visible_page: VisiblePage::Home,
            ..AppPreferences::default()
        };
        let second = AppPreferences {
            visible_page: VisiblePage::Status,
            ..first.clone()
        };
        let third = AppPreferences {
            visible_page: VisiblePage::Settings,
            ..first.clone()
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(WriterCommand::Write(second.clone())).unwrap();
        sender.send(WriterCommand::Write(third.clone())).unwrap();
        assert!(!drain_latest_write(&receiver, &mut first));
        assert_eq!(first, third);

        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(WriterCommand::Write(second.clone())).unwrap();
        sender.send(WriterCommand::Stop).unwrap();
        assert!(drain_latest_write(&receiver, &mut first));
        assert_eq!(first, second);

        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let changed = WindowObservation {
            restored_bounds: None,
            maximized: false,
            visible_page: VisiblePage::Settings,
        };
        assert_eq!(tracker.observe(Some(changed.clone())), None);
        assert!(tracker.observe(Some(changed.clone())).is_some());
        for _ in 0..4 {
            assert_eq!(tracker.observe(Some(changed.clone())), None);
        }
        assert_eq!(PREFERENCE_RETRY_DELAYS.len() + 1, 3);
        assert_eq!(
            retry_delay_after_failure(1),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay_after_failure(2),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(retry_delay_after_failure(3), None);
        assert_eq!(retry_delay_after_failure(4), None);
    }

    #[test]
    fn queued_restored_bounds_survive_maximize_before_ack_and_exit_flush() {
        let bounds_a = WindowBounds {
            x: 10,
            y: 20,
            width: 800,
            height: 650,
        };
        let bounds_b = WindowBounds {
            x: 300,
            y: 240,
            width: 980,
            height: 760,
        };
        let mut tracker = PersistenceTracker::new(AppPreferences {
            restored_bounds: Some(bounds_a),
            ..AppPreferences::default()
        });
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        assert_eq!(tracker.observe(Some(restored_b.clone())), None);
        let queued_b = tracker.observe(Some(restored_b)).expect("B queued");
        assert_eq!(queued_b.restored_bounds, Some(bounds_b.clone()));

        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Settings,
        };
        assert_eq!(tracker.observe(Some(maximized.clone())), None);
        let exit_snapshot = tracker.snapshot_for_flush(Some(&maximized));
        assert_eq!(exit_snapshot.restored_bounds, Some(bounds_b));
        assert!(exit_snapshot.maximized);
        assert_eq!(exit_snapshot.visible_page, VisiblePage::Settings);
    }

    #[test]
    fn older_saved_ack_does_not_roll_latest_desired_snapshot_back() {
        let bounds_a = WindowBounds {
            x: 10,
            y: 20,
            width: 800,
            height: 650,
        };
        let bounds_b = WindowBounds {
            x: 300,
            y: 240,
            width: 980,
            height: 760,
        };
        let initial = AppPreferences {
            restored_bounds: Some(bounds_a),
            ..AppPreferences::default()
        };
        let mut tracker = PersistenceTracker::new(initial);
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        tracker.observe(Some(restored_b.clone()));
        let queued_b = tracker.observe(Some(restored_b)).unwrap();
        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Status,
        };
        tracker.observe(Some(maximized));
        tracker.mark_persisted(queued_b);

        assert_eq!(tracker.latest_desired().restored_bounds, Some(bounds_b));
        assert!(tracker.latest_desired().maximized);
        assert_eq!(tracker.latest_desired().visible_page, VisiblePage::Status);
    }

    #[test]
    fn failed_in_flight_snapshot_remains_the_exit_restore_source() {
        let bounds_b = WindowBounds {
            x: -400,
            y: 80,
            width: 920,
            height: 700,
        };
        let mut tracker = PersistenceTracker::new(AppPreferences::default());
        let restored_b = WindowObservation {
            restored_bounds: Some(bounds_b.clone()),
            maximized: false,
            visible_page: VisiblePage::Home,
        };
        tracker.observe(Some(restored_b.clone()));
        assert!(tracker.observe(Some(restored_b)).is_some());
        for _failed_attempt in 0..3 {
            assert_eq!(
                tracker.latest_desired().restored_bounds,
                Some(bounds_b.clone())
            );
        }

        let exit_snapshot = tracker.snapshot_for_flush(Some(&WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Settings,
        }));
        assert_eq!(exit_snapshot.restored_bounds, Some(bounds_b));
        assert!(exit_snapshot.maximized);
    }

    #[test]
    fn writer_results_expose_only_bounded_safe_russian_error_and_success_clears_it() {
        assert_eq!(
            preference_error_for_result(&WriterResult::Failed),
            PREFERENCE_SAVE_ERROR_RU
        );
        assert_eq!(
            preference_error_for_result(&WriterResult::Saved(AppPreferences::default())),
            ""
        );
        assert!(!PREFERENCE_SAVE_ERROR_RU.is_ascii());
        assert!(PREFERENCE_SAVE_ERROR_RU.chars().count() < 64);
        assert!(!PREFERENCE_SAVE_ERROR_RU.contains("http"));
        assert!(!PREFERENCE_SAVE_ERROR_RU.contains(":\\"));
    }

    #[test]
    fn writer_shutdown_drains_pending_snapshots_in_order() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        let mut writer = PreferenceWriter::start(Some(store.clone()));
        writer.enqueue(AppPreferences {
            visible_page: VisiblePage::Status,
            ..AppPreferences::default()
        });
        writer.enqueue(AppPreferences {
            visible_page: VisiblePage::Settings,
            ..AppPreferences::default()
        });
        writer.stop();
        assert_eq!(store.load().unwrap().visible_page, VisiblePage::Settings);
    }

    #[test]
    #[ignore = "manual event-loop enqueue benchmark"]
    fn preference_enqueue_latency_baseline() {
        let mut writer = PreferenceWriter::start(None);
        let preferences = AppPreferences::default();
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            writer.enqueue(preferences.clone());
        }
        let elapsed = started.elapsed();
        println!(
            "preference enqueue: {:?} total, {:?} average",
            elapsed,
            elapsed / 1_000
        );
        writer.stop();
    }

    #[test]
    fn maximized_observation_preserves_last_restored_bounds() {
        let initial = AppPreferences {
            restored_bounds: Some(WindowBounds {
                x: 1,
                y: 2,
                width: 800,
                height: 650,
            }),
            ..AppPreferences::default()
        };
        let mut tracker = PersistenceTracker::new(initial.clone());
        let maximized = WindowObservation {
            restored_bounds: None,
            maximized: true,
            visible_page: VisiblePage::Status,
        };
        assert_eq!(tracker.observe(Some(maximized.clone())), None);
        let saved = tracker
            .observe(Some(maximized))
            .expect("stable maximized change");
        assert_eq!(saved.restored_bounds, initial.restored_bounds);
        assert!(saved.maximized);
        assert_eq!(saved.visible_page, VisiblePage::Status);
    }

    #[test]
    fn native_resize_and_persistence_source_contract() {
        let rust = include_str!("main.rs");
        let slint = include_str!("../ui/app.slint");
        assert!(rust.contains("ui.on_window_resize"));
        assert!(rust.contains("parse_resize_edge"));
        assert!(rust.contains("drag_resize_window"));
        assert!(rust.contains("PreferenceStore::from_local_app_data()"));
        assert!(rust.contains("TimerMode::Repeated, Duration::from_millis(500)"));
        assert!(rust.contains("stop_and_flush"));
        let exit_branch = rust
            .split("TrayCommand::Exit => {")
            .nth(1)
            .and_then(|source| source.split("}").next())
            .expect("tray Exit branch");
        assert!(
            exit_branch.find("flush_preferences").unwrap()
                < exit_branch.find("quit_event_loop").unwrap()
        );
        let timer = rust
            .split("fn wire_preference_persistence(")
            .nth(1)
            .and_then(|source| source.split("fn wire_updater").next())
            .expect("preference timer");
        assert!(timer.contains(".enqueue(preferences)"));
        assert!(!timer.contains(".save("));
        assert!(timer.contains("monitor_cap_changed"));
        let maximize = rust
            .split("ui.on_window_toggle_maximize")
            .nth(1)
            .and_then(|source| source.split("ui.on_window_close").next())
            .expect("maximize callback");
        assert!(
            maximize.find("set_current_monitor_max_size").unwrap()
                < maximize.find("set_maximized").unwrap()
        );
        assert!(!slint.contains("max-width: 1120px"));
        assert!(!slint.contains("max-height: 1000px"));
        assert!(slint.contains("callback window-resize(string)"));
        let visual_frame = slint
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|source| source.split("key-pressed(event)").next())
            .expect("inset visual frame");
        assert!(slint.contains("private property <length> resize-border: 6px;"));
        assert!(visual_frame.contains("x: root.resize-border;"));
        assert!(visual_frame.contains("y: root.resize-border;"));
        assert!(visual_frame.contains("width: parent.width - 2 * root.resize-border;"));
        assert!(visual_frame.contains("height: parent.height - 2 * root.resize-border;"));
        assert!(slint.contains("in property <string> preference-error;"));
        assert!(slint.contains("text: root.preference-error"));
        for edge in ["n", "ne", "e", "se", "s", "sw", "w", "nw"] {
            assert!(parse_resize_edge(edge).is_some(), "missing {edge}");
            assert!(
                slint.contains(&format!("root.window-resize(\"{edge}\")")),
                "missing Slint hit region for {edge}"
            );
        }
    }

    #[test]
    fn update_install_gate_accepts_only_idle_runtime_states() {
        assert!(runtime_allows_update(&UiState::Empty.presentation()));
        assert!(runtime_allows_update(
            &UiState::Ready {
                profile: "profile".into(),
                node: "auto".into(),
            }
            .presentation()
        ));
        assert!(!runtime_allows_update(
            &UiState::Connected {
                profile: "profile".into(),
                node: "node".into(),
            }
            .presentation()
        ));
        assert!(!runtime_allows_update(
            &UiState::Error {
                message: "x".into()
            }
            .presentation()
        ));
    }

    #[test]
    fn background_launch_and_restore_source_contract() {
        let source = include_str!("main.rs");
        assert!(source.contains("--background"));
        assert!(source.contains("LaunchMode::Background"));
        assert!(source.contains("slint::run_event_loop()"));
        assert!(source.contains("initial_window_visible(launch_mode, tray_runtime.is_some())"));

        let show_branch = source
            .split("TrayCommand::ShowWindow => {")
            .nth(1)
            .and_then(|source| source.split("TrayCommand::ToggleConnection").next())
            .expect("tray ShowWindow branch");
        let unminimize = show_branch
            .find("set_minimized(false)")
            .expect("restore must unminimize");
        let show = show_branch.find("ui.show()").expect("restore must show");
        assert!(unminimize < show, "unminimize must happen before show");
        assert!(show_branch.contains("window.set_visible(true)"));
        assert!(show_branch.contains("window.focus_window()"));
    }

    #[test]
    fn background_mode_stays_hidden_only_when_tray_is_available() {
        assert_eq!(
            parse_launch_arguments(["--background".into()])
                .unwrap()
                .mode,
            LaunchMode::Background
        );
        assert_eq!(
            parse_launch_arguments(Vec::new()).unwrap().mode,
            LaunchMode::Foreground
        );
        assert!(!initial_window_visible(LaunchMode::Background, true));
        assert!(initial_window_visible(LaunchMode::Background, false));
        assert!(initial_window_visible(LaunchMode::Foreground, true));
    }

    #[test]
    fn preview_marker_is_a_foreground_only_capture_argument() {
        assert_eq!(
            parse_launch_arguments(["--quiet-signal-preview".into()]),
            Ok(LaunchArguments::foreground())
        );
    }

    #[test]
    fn installer_uri_decodes_one_strict_http_subscription_url() {
        let parsed = parse_launch_arguments([
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Da%252Fb".into(),
        ])
        .unwrap();

        assert_eq!(
            parsed,
            LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some("https://example.com/sub?token=a%2Fb".into()),
            }
        );
    }

    #[test]
    fn installer_uri_rejects_malformed_duplicate_and_non_http_inputs() {
        for argument in [
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2F%ZZ",
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2F%2",
            "multicore://install-sub?url=https%3A%2F%2Fa.example&url=https%3A%2F%2Fb.example",
            "multicore://install-sub?url=ftp%3A%2F%2Fexample.com%2Fsub",
            "multicore://install-sub?url=https://example.com/sub",
            "multicore://install-sub?other=https%3A%2F%2Fexample.com",
        ] {
            assert!(
                parse_launch_arguments([argument.into()]).is_err(),
                "{argument}"
            );
        }
    }

    #[test]
    fn background_and_installer_uri_are_mutually_exclusive() {
        let uri = std::ffi::OsString::from(
            "multicore://install-sub?url=https%3A%2F%2Fexample.com%2Fsubscription",
        );
        assert!(parse_launch_arguments(["--background".into(), uri.clone()]).is_err());
        assert!(parse_launch_arguments([uri, "--background".into()]).is_err());
    }

    #[test]
    fn secondary_launch_preserves_foreground_background_and_deep_link_intent() {
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments::foreground()),
            Activation::Show
        );
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments {
                mode: LaunchMode::Background,
                subscription_url: None,
            }),
            Activation::Background
        );
        assert_eq!(
            activation_for_launch_arguments(&LaunchArguments {
                mode: LaunchMode::Foreground,
                subscription_url: Some("https://example.com/sub".into()),
            }),
            Activation::InstallSubscription("https://example.com/sub".into())
        );
    }

    #[test]
    fn single_instance_gate_precedes_daemon_bootstrap() {
        let source = include_str!("main.rs");
        let claim = source
            .find("single_instance::claim_or_forward")
            .expect("single-instance gate");
        let bootstrap = source.find("bootstrap::start()").expect("daemon bootstrap");
        assert!(claim < bootstrap);
    }

    #[test]
    fn installer_launch_only_prefills_and_reveals_the_editor() {
        let source = include_str!("main.rs");
        let setup = source
            .split("if let Some(subscription_url) = launch_arguments.subscription_url {")
            .nth(1)
            .and_then(|source| source.split("apply_update_state(").next())
            .expect("installer launch UI setup");
        assert!(setup.contains("ui.set_local_page(\"subscriptions\".into())"));
        assert!(setup.contains("ui.set_subscription_draft(subscription_url.into())"));
        assert!(setup.contains("ui.set_replace_editor_open(true)"));
        assert!(!setup.contains("invoke_import_url"));
        assert!(!setup.contains("run_import"));

        let secondary = source
            .split("single_instance::Activation::InstallSubscription(url) => {")
            .nth(1)
            .and_then(|source| source.split(".is_err()").next())
            .expect("secondary installer activation");
        assert!(secondary.contains("model.open_panel(Panel::None)"));
        assert!(
            secondary.find("model.open_panel(Panel::None)").unwrap()
                < secondary.find("prefill_subscription(&ui, url)").unwrap()
        );
    }

    #[test]
    fn first_run_clipboard_action_reuses_the_import_pipeline() {
        let source = include_str!("main.rs");
        assert!(source.contains("wire_clipboard_import(&ui, model.clone());"));

        let wiring = source
            .split("fn wire_clipboard_import(")
            .nth(1)
            .and_then(|source| source.split("fn wire_subscription_refresh(").next())
            .expect("clipboard import wiring");
        for required in [
            "ui.on_paste_subscription",
            "ui.on_dismiss_first_run_import",
            "clipboard_text::read_subscription_text()",
            "ui.set_subscription_draft",
            "ui.invoke_import_url",
            "model.set_import_error_message(error.user_message())",
            "apply_snapshot(&ui, &current)",
            "model.dismiss_import_error()",
        ] {
            assert!(
                wiring.contains(required),
                "missing clipboard wiring: {required}"
            );
        }
    }
}
