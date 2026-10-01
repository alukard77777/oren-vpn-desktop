#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    ShowWindow,
    ToggleConnection,
    SelectGroup { group_id: String },
    SelectNode { group_id: String, node_id: String },
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayGroup {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) icon: String,
    /// Downloaded group icon image, drawn instead of `icon` when present.
    pub(crate) icon_id: Option<String>,
    pub(crate) selected_node_label: String,
    /// The group the popup shows (the one selected on the home screen).
    pub(crate) selected: bool,
    pub(crate) nodes: Vec<TrayNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayNode {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) icon: String,
    pub(crate) icon_id: Option<String>,
    pub(crate) selected: bool,
    pub(crate) latency_text: String,
    /// "success" | "warning" | "danger" | "neutral", as on the home route list.
    pub(crate) latency_tone: String,
    /// Re-measured on its own (right click): the row shows the placeholder pill.
    pub(crate) measuring: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayInput {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) connected: bool,
    pub(crate) has_profile: bool,
    pub(crate) groups: Vec<TrayGroup>,
    pub(crate) route_selection_enabled: bool,
    /// The provider's dumb-mode: one group, no group tabs.
    pub(crate) dumb_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuEntry {
    pub(crate) icon: String,
    pub(crate) icon_id: Option<String>,
    pub(crate) measuring: bool,
    pub(crate) label: String,
    /// Groups: the selected node's label. Nodes: the latency text.
    pub(crate) detail: String,
    /// Nodes: the latency tone; groups: empty.
    pub(crate) tone: String,
    /// Groups: shown in the popup. Nodes: the selected route.
    pub(crate) checked: bool,
    pub(crate) enabled: bool,
    pub(crate) command: TrayCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuModel {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) connected: bool,
    pub(crate) has_profile: bool,
    pub(crate) groups: Vec<TrayMenuEntry>,
    pub(crate) nodes: Vec<TrayMenuEntry>,
    pub(crate) dumb_mode: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrayPopupModelCache {
    last: Option<TrayMenuModel>,
}

impl TrayPopupModelCache {
    pub(crate) fn observe(&mut self, model: &TrayMenuModel) -> bool {
        if self.last.as_ref() == Some(model) {
            false
        } else {
            self.last = Some(model.clone());
            true
        }
    }
}

pub(crate) fn build_menu_model(input: TrayInput) -> TrayMenuModel {
    let mut nodes = Vec::new();
    let groups = input
        .groups
        .into_iter()
        .map(|group| {
            let group_id = group.id;
            nodes.extend(group.nodes.into_iter().map(|node| TrayMenuEntry {
                icon: node.icon,
                icon_id: node.icon_id,
                measuring: node.measuring,
                label: node.label,
                detail: node.latency_text,
                tone: node.latency_tone,
                checked: node.selected,
                enabled: input.route_selection_enabled,
                command: TrayCommand::SelectNode {
                    group_id: group_id.clone(),
                    node_id: node.id,
                },
            }));
            TrayMenuEntry {
                icon: group.icon,
                icon_id: group.icon_id,
                measuring: false,
                label: group.label,
                detail: group.selected_node_label,
                tone: String::new(),
                checked: group.selected,
                enabled: input.route_selection_enabled,
                command: TrayCommand::SelectGroup { group_id },
            }
        })
        .collect();

    TrayMenuModel {
        status: input.status,
        connection_label: input.connection_label,
        connection_enabled: input.connection_enabled,
        connected: input.connected,
        has_profile: input.has_profile,
        groups,
        nodes,
        dumb_mode: input.dumb_mode,
    }
}

#[cfg(windows)]
mod native {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use slint::{Timer, TimerMode};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::{
        TrayInteraction, TrayMenuModel, TrayPointerButton, TrayPointerState, pointer_interaction,
    };

    const TRAY_GUID: u128 = 0x8ca0_87f4_4514_4ce5_91f1_7c6c_e181_46d1;

    pub(crate) struct TrayRuntime {
        _tray: Rc<RefCell<NativeTray>>,
        _timer: Timer,
    }

    struct NativeTray {
        icon: TrayIcon,
        model: TrayMenuModel,
    }

    impl NativeTray {
        fn new(model: TrayMenuModel) -> Result<Self, String> {
            let icon = Icon::from_rgba(
                crate::app_icon::tray_rgba(),
                crate::app_icon::TRAY_ICON_SIZE,
                crate::app_icon::TRAY_ICON_SIZE,
            )
            .map_err(|error| format!("invalid tray icon: {error}"))?;
            let tray = TrayIconBuilder::new()
                .with_id("multicore-main")
                .with_guid(TRAY_GUID)
                .with_tooltip(tooltip(&model))
                .with_icon(icon)
                .with_menu_on_left_click(false)
                .build()
                .map_err(|error| format!("failed to create tray icon: {error}"))?;
            Ok(Self { icon: tray, model })
        }

        fn sync(&mut self, model: TrayMenuModel) {
            if self.model == model {
                return;
            }
            let _ = self.icon.set_tooltip(Some(tooltip(&model)));
            self.model = model;
        }

        fn drain_events(&self) -> Vec<TrayInteraction> {
            let mut events = Vec::new();
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    position,
                    button,
                    button_state,
                    ..
                } = event
                {
                    let button = match button {
                        MouseButton::Left => TrayPointerButton::Left,
                        MouseButton::Right => TrayPointerButton::Right,
                        MouseButton::Middle => TrayPointerButton::Middle,
                    };
                    let state = match button_state {
                        MouseButtonState::Down => TrayPointerState::Down,
                        MouseButtonState::Up => TrayPointerState::Up,
                    };
                    if let Some(interaction) = pointer_interaction(
                        button,
                        state,
                        (position.x.round() as i32, position.y.round() as i32),
                    ) {
                        events.push(interaction);
                    }
                }
            }
            events
        }
    }

    pub(crate) fn start<Model, Action>(
        initial: TrayMenuModel,
        model_provider: Model,
        action_handler: Action,
    ) -> Result<TrayRuntime, String>
    where
        Model: Fn() -> TrayMenuModel + 'static,
        Action: Fn(TrayInteraction) + 'static,
    {
        let tray = Rc::new(RefCell::new(NativeTray::new(initial)?));
        let timer = Timer::default();
        let timer_tray = tray.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(180), move || {
            let model = model_provider();
            let events = {
                let mut tray = timer_tray.borrow_mut();
                tray.sync(model);
                tray.drain_events()
            };
            for event in events {
                action_handler(event);
            }
        });
        Ok(TrayRuntime {
            _tray: tray,
            _timer: timer,
        })
    }

    fn tooltip(model: &TrayMenuModel) -> String {
        let value = format!("MULTICORE — {}", model.status);
        value.chars().take(120).collect()
    }
}

#[cfg(windows)]
pub(crate) use native::start;

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) struct TrayRuntime;

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn start<Model, Action>(
    _initial: TrayMenuModel,
    _model_provider: Model,
    _action_handler: Action,
) -> Result<TrayRuntime, String>
where
    Model: Fn() -> TrayMenuModel + 'static,
    Action: Fn(TrayInteraction) + 'static,
{
    Err("system tray is available only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::{
        TrayCommand, TrayGroup, TrayInput, TrayInteraction, TrayNode, TrayPointerButton,
        TrayPointerState, TrayPopupModelCache, build_menu_model, pointer_interaction,
    };

    #[test]
    fn menu_exposes_every_groups_nodes_and_icons_without_treating_groups_as_alternatives() {
        let input = TrayInput {
            status: "В сети".into(),
            connection_label: "Отключиться".into(),
            connection_enabled: true,
            connected: true,
            has_profile: true,
            groups: vec![
                TrayGroup {
                    id: "server:opaque".into(),
                    label: "Сервер".into(),
                    icon: "🌍".into(),
                    icon_id: None,
                    selected_node_label: "Швеция".into(),
                    selected: true,
                    nodes: vec![TrayNode {
                        id: "node:a".into(),
                        label: "Швеция".into(),
                        icon: "🇸🇪".into(),
                        selected: true,
                        latency_text: "42 ms".into(),
                        latency_tone: "success".into(),
                        icon_id: None,
                        measuring: false,
                    }],
                },
                TrayGroup {
                    id: "games:opaque".into(),
                    label: "Игры".into(),
                    icon: "🎮".into(),
                    icon_id: None,
                    selected_node_label: "Direct".into(),
                    selected: false,
                    nodes: vec![TrayNode {
                        id: "node:b".into(),
                        label: "Direct".into(),
                        icon: "↗".into(),
                        selected: true,
                        latency_text: String::new(),
                        latency_tone: "neutral".into(),
                        icon_id: None,
                        measuring: false,
                    }],
                },
            ],
            route_selection_enabled: true,
            dumb_mode: false,
        };

        let model = build_menu_model(input);
        assert_eq!(model.status, "В сети");
        assert_eq!(model.connection_label, "Отключиться");
        assert_eq!(model.groups.len(), 2);
        assert_eq!(model.groups[0].detail, "Швеция");
        assert_eq!(model.groups[1].detail, "Direct");
        assert_eq!(model.groups[0].icon, "🌍");
        assert_eq!(model.nodes.len(), 2);
        assert_eq!(model.nodes[0].icon, "🇸🇪");
        // The shown group and each node's latency (text + tone) reach the popup.
        assert!(model.groups[0].checked);
        assert!(!model.groups[1].checked);
        assert_eq!(model.nodes[0].detail, "42 ms");
        assert_eq!(model.nodes[0].tone, "success");
        assert_eq!(
            model.nodes[0].command,
            TrayCommand::SelectNode {
                group_id: "server:opaque".into(),
                node_id: "node:a".into(),
            }
        );
        assert_eq!(
            model.nodes[1].command,
            TrayCommand::SelectNode {
                group_id: "games:opaque".into(),
                node_id: "node:b".into(),
            }
        );
    }

    #[test]
    fn empty_groups_never_leak_nodes() {
        let model = build_menu_model(TrayInput {
            status: "Готово".into(),
            connection_label: "Подключиться".into(),
            connection_enabled: false,
            connected: false,
            has_profile: false,
            groups: Vec::new(),
            route_selection_enabled: true,
            dumb_mode: false,
        });

        assert!(model.nodes.is_empty());
        assert!(!model.connection_enabled);
    }

    #[test]
    fn popup_model_cache_suppresses_identical_timer_rebinds() {
        let model = build_menu_model(TrayInput {
            status: "Готово".into(),
            connection_label: "Подключиться".into(),
            connection_enabled: true,
            connected: false,
            has_profile: true,
            groups: Vec::new(),
            route_selection_enabled: true,
            dumb_mode: false,
        });
        let mut cache = TrayPopupModelCache::default();

        assert!(cache.observe(&model));
        assert!(!cache.observe(&model));

        let mut changed = model;
        changed.status = "В сети".into();
        assert!(cache.observe(&changed));
        assert!(!cache.observe(&changed));
    }

    #[test]
    fn left_button_release_opens_the_main_window() {
        assert_eq!(
            pointer_interaction(
                TrayPointerButton::Left,
                TrayPointerState::Up,
                (1_900, 1_040),
            ),
            Some(TrayInteraction::Command(TrayCommand::ShowWindow))
        );
    }

    #[test]
    fn right_button_release_opens_the_branded_menu_at_the_pointer() {
        assert_eq!(
            pointer_interaction(TrayPointerButton::Right, TrayPointerState::Up, (-24, 1_040),),
            Some(TrayInteraction::ShowMenu { x: -24, y: 1_040 })
        );
    }

    #[test]
    fn pointer_down_and_middle_button_do_nothing() {
        assert_eq!(
            pointer_interaction(TrayPointerButton::Left, TrayPointerState::Down, (10, 20),),
            None
        );
        assert_eq!(
            pointer_interaction(TrayPointerButton::Middle, TrayPointerState::Up, (10, 20),),
            None
        );
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayPointerButton {
    Left,
    Right,
    Middle,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayPointerState {
    Down,
    Up,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrayInteraction {
    Command(TrayCommand),
    ShowMenu { x: i32, y: i32 },
}

#[cfg(any(windows, test))]
pub(crate) fn pointer_interaction(
    button: TrayPointerButton,
    state: TrayPointerState,
    position: (i32, i32),
) -> Option<TrayInteraction> {
    if state != TrayPointerState::Up {
        return None;
    }

    match button {
        TrayPointerButton::Left => Some(TrayInteraction::Command(TrayCommand::ShowWindow)),
        TrayPointerButton::Right => Some(TrayInteraction::ShowMenu {
            x: position.0,
            y: position.1,
        }),
        TrayPointerButton::Middle => None,
    }
}

#[cfg(not(windows))]
const MAX_NATIVE_LABEL_CHARS: usize = 96;

#[cfg(not(windows))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeMenuItem {
    Label(String),
    Separator,
    Action {
        id: String,
        label: String,
        enabled: bool,
        command: TrayCommand,
    },
    Check {
        id: String,
        label: String,
        checked: bool,
        enabled: bool,
        command: TrayCommand,
    },
    Submenu {
        label: String,
        enabled: bool,
        items: Vec<NativeMenuItem>,
    },
}

#[cfg(not(windows))]
fn native_label(icon: &str, label: &str, detail: &str) -> String {
    let mut text = if icon.is_empty() {
        label.to_owned()
    } else {
        format!("{icon} {label}")
    };
    if !detail.is_empty() {
        text.push_str(" · ");
        text.push_str(detail);
    }
    text.chars().take(MAX_NATIVE_LABEL_CHARS).collect()
}

#[cfg(not(windows))]
pub(crate) fn native_menu_plan(model: &TrayMenuModel) -> Vec<NativeMenuItem> {
    let mut items = vec![
        NativeMenuItem::Label(native_label("", "MULTICORE", &model.status)),
        NativeMenuItem::Separator,
        NativeMenuItem::Action {
            id: "show".into(),
            label: "Открыть MULTICORE".into(),
            enabled: true,
            command: TrayCommand::ShowWindow,
        },
        NativeMenuItem::Action {
            id: "connection".into(),
            label: native_label("", &model.connection_label, ""),
            enabled: model.connection_enabled,
            command: TrayCommand::ToggleConnection,
        },
    ];
    let groups = model
        .groups
        .iter()
        .filter_map(|group| {
            let TrayCommand::SelectGroup { group_id } = &group.command else {
                return None;
            };
            let nodes = model
                .nodes
                .iter()
                .filter_map(|node| match &node.command {
                    TrayCommand::SelectNode {
                        group_id: node_group,
                        node_id,
                    } if node_group == group_id => Some(NativeMenuItem::Check {
                        id: format!("node\u{1f}{group_id}\u{1f}{node_id}"),
                        label: native_label(&node.icon, &node.label, ""),
                        checked: node.checked,
                        enabled: node.enabled,
                        command: node.command.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            Some(NativeMenuItem::Submenu {
                label: native_label(&group.icon, &group.label, &group.detail),
                enabled: group.enabled && !nodes.is_empty(),
                items: nodes,
            })
        })
        .collect::<Vec<_>>();
    if !groups.is_empty() {
        items.push(NativeMenuItem::Separator);
        items.extend(groups);
    }
    items.push(NativeMenuItem::Separator);
    items.push(NativeMenuItem::Action {
        id: "exit".into(),
        label: "Выйти".into(),
        enabled: true,
        command: TrayCommand::Exit,
    });
    items
}

#[cfg(not(windows))]
pub(crate) fn native_menu_commands(
    items: &[NativeMenuItem],
) -> std::collections::HashMap<String, TrayCommand> {
    let mut commands = std::collections::HashMap::new();
    for item in items {
        match item {
            NativeMenuItem::Action { id, command, .. }
            | NativeMenuItem::Check { id, command, .. } => {
                commands.insert(id.clone(), command.clone());
            }
            NativeMenuItem::Submenu { items, .. } => commands.extend(native_menu_commands(items)),
            NativeMenuItem::Label(_) | NativeMenuItem::Separator => {}
        }
    }
    commands
}

#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IconArea {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[cfg(not(windows))]
pub(crate) fn status_icon_anchor(
    icon: IconArea,
    monitor_top: i32,
    monitor_height: i32,
    scale: i32,
) -> (i32, i32) {
    let icon_center = icon.y + icon.height / 2;
    let edge = if icon_center < monitor_top + monitor_height / 2 {
        icon.y + icon.height
    } else {
        icon.y
    };
    let scale = scale.max(1);
    ((icon.x + icon.width) * scale, edge * scale)
}

#[cfg(all(test, not(windows)))]
mod native_menu_tests {
    use super::{
        NativeMenuItem, TrayCommand, TrayGroup, TrayInput, TrayNode, build_menu_model,
        native_menu_commands, native_menu_plan,
    };

    fn input(connected: bool, routes_enabled: bool) -> TrayInput {
        TrayInput {
            status: if connected {
                "В сети"
            } else {
                "Готово"
            }
            .into(),
            connection_label: if connected {
                "Отключиться"
            } else {
                "Подключиться"
            }
            .into(),
            connection_enabled: true,
            connected,
            has_profile: true,
            groups: vec![
                TrayGroup {
                    id: "g:server".into(),
                    label: "Сервер".into(),
                    icon: "🌍".into(),
                    icon_id: None,
                    selected_node_label: "Швеция".into(),
                    selected: true,
                    nodes: vec![
                        TrayNode {
                            id: "n:se".into(),
                            label: "Швеция".into(),
                            icon: "🇸🇪".into(),
                            icon_id: None,
                            selected: true,
                            latency_text: "42 ms".into(),
                            latency_tone: "success".into(),
                            measuring: false,
                        },
                        TrayNode {
                            id: "n:fi".into(),
                            label: "Финляндия".into(),
                            icon: "🇫🇮".into(),
                            icon_id: None,
                            selected: false,
                            latency_text: String::new(),
                            latency_tone: "neutral".into(),
                            measuring: false,
                        },
                    ],
                },
                TrayGroup {
                    id: "g:empty".into(),
                    label: "Пусто".into(),
                    icon: String::new(),
                    icon_id: None,
                    selected_node_label: String::new(),
                    selected: false,
                    nodes: Vec::new(),
                },
            ],
            route_selection_enabled: routes_enabled,
            dumb_mode: false,
        }
    }

    #[test]
    fn plan_orders_status_actions_routes_and_exit() {
        let plan = native_menu_plan(&build_menu_model(input(true, true)));
        assert_eq!(plan[0], NativeMenuItem::Label("MULTICORE · В сети".into()));
        assert_eq!(plan[1], NativeMenuItem::Separator);
        assert!(matches!(
            &plan[2],
            NativeMenuItem::Action { id, command: TrayCommand::ShowWindow, enabled: true, .. } if id == "show"
        ));
        assert!(matches!(
            &plan[3],
            NativeMenuItem::Action { label, command: TrayCommand::ToggleConnection, .. } if label == "Отключиться"
        ));
        assert_eq!(plan[4], NativeMenuItem::Separator);
        let NativeMenuItem::Submenu {
            label,
            enabled,
            items,
        } = &plan[5]
        else {
            panic!("expected the server submenu");
        };
        assert_eq!(label, "🌍 Сервер · Швеция");
        assert!(*enabled);
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[0],
            NativeMenuItem::Check { label, checked: true, enabled: true, .. } if label == "🇸🇪 Швеция"
        ));
        assert!(matches!(
            &items[1],
            NativeMenuItem::Check { checked: false, .. }
        ));
        assert!(matches!(
            &plan[6],
            NativeMenuItem::Submenu { enabled: false, items, .. } if items.is_empty()
        ));
        assert_eq!(plan[7], NativeMenuItem::Separator);
        assert!(matches!(
            plan.last(),
            Some(NativeMenuItem::Action {
                command: TrayCommand::Exit,
                ..
            })
        ));
    }

    #[test]
    fn route_items_follow_selection_availability_and_map_to_exact_commands() {
        let plan = native_menu_plan(&build_menu_model(input(false, false)));
        let NativeMenuItem::Submenu { enabled, items, .. } = &plan[5] else {
            panic!("expected the server submenu");
        };
        assert!(!*enabled);
        assert!(
            items
                .iter()
                .all(|item| matches!(item, NativeMenuItem::Check { enabled: false, .. }))
        );

        let commands = native_menu_commands(&plan);
        assert_eq!(commands["show"], TrayCommand::ShowWindow);
        assert_eq!(commands["connection"], TrayCommand::ToggleConnection);
        assert_eq!(commands["exit"], TrayCommand::Exit);
        assert_eq!(
            commands["node\u{1f}g:server\u{1f}n:fi"],
            TrayCommand::SelectNode {
                group_id: "g:server".into(),
                node_id: "n:fi".into(),
            }
        );
        assert_eq!(commands.len(), 5);
    }

    #[test]
    fn status_icon_anchor_uses_the_icon_edge_facing_the_screen_center() {
        use super::{IconArea, status_icon_anchor};

        let top_bar = IconArea {
            x: 1_880,
            y: 2,
            width: 18,
            height: 18,
        };
        assert_eq!(status_icon_anchor(top_bar, 0, 1_080, 1), (1_898, 20));
        let bottom_bar = IconArea {
            x: 1_880,
            y: 1_050,
            width: 24,
            height: 24,
        };
        assert_eq!(status_icon_anchor(bottom_bar, 0, 1_080, 1), (1_904, 1_050));
        assert_eq!(status_icon_anchor(top_bar, 0, 1_080, 2), (3_796, 40));
        assert_eq!(status_icon_anchor(top_bar, 0, 1_080, 0), (1_898, 20));
    }

    #[test]
    fn plan_without_routes_has_no_route_section_and_bounds_labels() {
        let mut routeless = input(false, true);
        routeless.groups.clear();
        routeless.status = "x".repeat(300);
        let plan = native_menu_plan(&build_menu_model(routeless));
        assert_eq!(plan.len(), 6);
        let NativeMenuItem::Label(label) = &plan[0] else {
            panic!("expected the status label");
        };
        assert_eq!(label.chars().count(), 96);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use gtk::glib;
    use gtk::glib::object::ObjectType;
    use gtk::prelude::{DeviceExt, MonitorExt, SeatExt};
    use slint::{Timer, TimerMode};
    use tray_icon::menu::{
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    };
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    use super::{
        IconArea, NativeMenuItem, TrayCommand, TrayInteraction, TrayMenuModel,
        native_menu_commands, native_menu_plan, status_icon_anchor,
    };

    const START_TIMEOUT: Duration = Duration::from_secs(5);
    const SYNC_INTERVAL: Duration = Duration::from_millis(250);
    const EVENT_INTERVAL: Duration = Duration::from_millis(180);
    const STATUS_NOTIFIER_WATCHER: &str = "org.kde.StatusNotifierWatcher";

    const STATUS_ICON_SIZE: i32 = 64;

    #[derive(Default)]
    struct Shared {
        latest: Mutex<Option<TrayMenuModel>>,
        commands: Mutex<HashMap<String, TrayCommand>>,
        interactions: Mutex<Vec<TrayInteraction>>,
        stop: AtomicBool,
    }

    struct StatusIcon(glib::Object);

    impl StatusIcon {
        fn new(pixbuf: &gtk::gdk_pixbuf::Pixbuf) -> Self {
            use glib::translate::{FromGlibPtrFull, ToGlibPtr};
            let raw = unsafe { gtk::ffi::gtk_status_icon_new_from_pixbuf(pixbuf.to_glib_none().0) };
            Self(unsafe { glib::Object::from_glib_full(raw.cast::<glib::gobject_ffi::GObject>()) })
        }

        fn raw(&self) -> *mut gtk::ffi::GtkStatusIcon {
            self.0.as_ptr().cast()
        }

        fn set_title(&self, title: &str) {
            if let Ok(title) = std::ffi::CString::new(title) {
                unsafe { gtk::ffi::gtk_status_icon_set_title(self.raw(), title.as_ptr()) };
            }
        }

        fn set_tooltip(&self, text: &str) {
            if let Ok(text) = std::ffi::CString::new(text) {
                unsafe { gtk::ffi::gtk_status_icon_set_tooltip_text(self.raw(), text.as_ptr()) };
            }
        }

        fn set_visible(&self, visible: bool) {
            unsafe { gtk::ffi::gtk_status_icon_set_visible(self.raw(), visible.into()) };
        }

        fn area(&self) -> Option<IconArea> {
            let mut area = gtk::gdk::ffi::GdkRectangle {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            };
            let found = unsafe {
                gtk::ffi::gtk_status_icon_get_geometry(
                    self.raw(),
                    std::ptr::null_mut(),
                    &mut area,
                    std::ptr::null_mut(),
                )
            };
            (found != 0 && area.width > 0 && area.height > 0).then_some(IconArea {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
            })
        }

        fn connect_click(&self, signal: &str, shared: Arc<Shared>) {
            use glib::prelude::ObjectExt;
            let icon = self.0.downgrade();
            self.0.connect_local(signal, false, move |_| {
                if let Some(icon) = icon.upgrade() {
                    request_popup(&StatusIcon(icon), &shared);
                }
                None
            });
        }
    }

    enum NativeTray {
        StatusIcon(StatusIcon),
        Indicator(TrayIcon),
    }

    impl NativeTray {
        fn sync(&self, model: &TrayMenuModel, shared: &Shared) {
            match self {
                Self::StatusIcon(icon) => icon.set_tooltip(&tooltip(model)),
                Self::Indicator(tray) => {
                    if let Ok(menu) = build_menu(model, shared) {
                        tray.set_menu(Some(Box::new(menu)));
                    }
                    let _ = tray.set_tooltip(Some(tooltip(model)));
                }
            }
        }
    }

    pub(crate) struct TrayRuntime {
        shared: Arc<Shared>,
        _timer: Timer,
    }

    impl Drop for TrayRuntime {
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn start<Model, Action>(
        initial: TrayMenuModel,
        model_provider: Model,
        action_handler: Action,
    ) -> Result<TrayRuntime, String>
    where
        Model: Fn() -> TrayMenuModel + 'static,
        Action: Fn(TrayInteraction) + 'static,
    {
        let shared = Arc::new(Shared::default());
        let (ready, started) = mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("multicore-tray".into())
            .spawn(move || run_gtk(initial, thread_shared, ready))
            .map_err(|error| format!("tray thread failed: {error}"))?;
        started
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| "tray thread did not report readiness".to_owned())??;

        let timer = Timer::default();
        let timer_shared = shared.clone();
        timer.start(TimerMode::Repeated, EVENT_INTERVAL, move || {
            if let Ok(mut latest) = timer_shared.latest.lock() {
                *latest = Some(model_provider());
            }
            let interactions = timer_shared
                .interactions
                .lock()
                .map(|mut queued| std::mem::take(&mut *queued))
                .unwrap_or_default();
            for interaction in interactions {
                action_handler(interaction);
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let command = timer_shared
                    .commands
                    .lock()
                    .ok()
                    .and_then(|commands| commands.get(event.id().as_ref()).cloned());
                if let Some(command) = command {
                    action_handler(TrayInteraction::Command(command));
                }
            }
        });
        Ok(TrayRuntime {
            shared,
            _timer: timer,
        })
    }

    fn run_gtk(
        initial: TrayMenuModel,
        shared: Arc<Shared>,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        let tray = match create_tray(&initial, &shared) {
            Ok(tray) => tray,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        let mut current = initial;
        glib::timeout_add_local(SYNC_INTERVAL, move || {
            if shared.stop.load(Ordering::SeqCst) {
                gtk::main_quit();
                return glib::ControlFlow::Break;
            }
            let latest = shared
                .latest
                .lock()
                .ok()
                .and_then(|mut latest| latest.take());
            if let Some(model) = latest
                && model != current
            {
                tray.sync(&model, &shared);
                current = model;
            }
            glib::ControlFlow::Continue
        });
        gtk::main();
    }

    fn create_tray(model: &TrayMenuModel, shared: &Arc<Shared>) -> Result<NativeTray, String> {
        gtk::init().map_err(|error| format!("GTK is unavailable: {error}"))?;
        if status_notifier_watcher_running() {
            return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                create_indicator(model, shared)
            }))
            .map_err(|_| "the AppIndicator library is not installed".to_owned())?
            .map(NativeTray::Indicator);
        }
        if gtk_uses_x11() && xembed_tray_running() {
            return Ok(NativeTray::StatusIcon(create_status_icon(model, shared)));
        }
        Err("no system tray host is running".into())
    }

    fn gtk_uses_x11() -> bool {
        use glib::prelude::ObjectExt;
        gtk::gdk::Display::default()
            .is_some_and(|display| display.type_().name() == "GdkX11Display")
    }

    fn create_status_icon(model: &TrayMenuModel, shared: &Arc<Shared>) -> StatusIcon {
        let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
            &glib::Bytes::from_owned(crate::app_icon::rgba(STATUS_ICON_SIZE as u32)),
            gtk::gdk_pixbuf::Colorspace::Rgb,
            true,
            8,
            STATUS_ICON_SIZE,
            STATUS_ICON_SIZE,
            STATUS_ICON_SIZE * 4,
        );
        let icon = StatusIcon::new(&pixbuf);
        icon.set_title("MULTICORE");
        icon.set_tooltip(&tooltip(model));
        icon.set_visible(true);
        icon.connect_click("activate", shared.clone());
        icon.connect_click("popup-menu", shared.clone());
        icon
    }

    fn request_popup(icon: &StatusIcon, shared: &Shared) {
        if let Some((x, y)) = popup_anchor(icon)
            && let Ok(mut queued) = shared.interactions.lock()
        {
            queued.push(TrayInteraction::ShowMenu { x, y });
        }
    }

    fn popup_anchor(icon: &StatusIcon) -> Option<(i32, i32)> {
        let display = gtk::gdk::Display::default()?;
        if let Some(area) = icon.area()
            && let Some(monitor) = display.monitor_at_point(area.x, area.y)
        {
            let bounds = monitor.geometry();
            return Some(status_icon_anchor(
                area,
                bounds.y(),
                bounds.height(),
                monitor.scale_factor(),
            ));
        }
        let (_, x, y) = display.default_seat()?.pointer()?.position();
        Some((x, y))
    }

    fn create_indicator(model: &TrayMenuModel, shared: &Shared) -> Result<TrayIcon, String> {
        let icon = Icon::from_rgba(crate::app_icon::rgba(32), 32, 32)
            .map_err(|error| format!("invalid tray icon: {error}"))?;
        TrayIconBuilder::new()
            .with_id("multicore-main")
            .with_title("MULTICORE")
            .with_tooltip(tooltip(model))
            .with_icon(icon)
            .with_menu(Box::new(build_menu(model, shared)?))
            .build()
            .map_err(|error| format!("failed to create tray icon: {error}"))
    }

    fn status_notifier_watcher_running() -> bool {
        let Ok(connection) =
            gtk::gio::bus_get_sync(gtk::gio::BusType::Session, None::<&gtk::gio::Cancellable>)
        else {
            return false;
        };
        connection
            .call_sync(
                Some(STATUS_NOTIFIER_WATCHER),
                "/StatusNotifierWatcher",
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&(STATUS_NOTIFIER_WATCHER, "IsStatusNotifierHostRegistered").into()),
                Some(glib::VariantTy::new("(v)").expect("static variant type")),
                gtk::gio::DBusCallFlags::NO_AUTO_START,
                1000,
                None::<&gtk::gio::Cancellable>,
            )
            .ok()
            .and_then(|reply| reply.get::<(glib::Variant,)>())
            .and_then(|(host_registered,)| host_registered.get::<bool>())
            .unwrap_or(false)
    }

    fn xembed_tray_running() -> bool {
        use x11rb::protocol::xproto::ConnectionExt;

        let Ok((connection, screen)) = x11rb::connect(None) else {
            return false;
        };
        let selection = format!("_NET_SYSTEM_TRAY_S{screen}");
        connection
            .intern_atom(false, selection.as_bytes())
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|atom| connection.get_selection_owner(atom.atom).ok())
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.owner != x11rb::NONE)
    }

    fn build_menu(model: &TrayMenuModel, shared: &Shared) -> Result<Menu, String> {
        let plan = native_menu_plan(model);
        let menu = Menu::new();
        for item in &plan {
            let built = build_item(item)?;
            menu.append(built.as_ref())
                .map_err(|error| format!("failed to build tray menu: {error}"))?;
        }
        if let Ok(mut commands) = shared.commands.lock() {
            *commands = native_menu_commands(&plan);
        }
        Ok(menu)
    }

    fn build_item(item: &NativeMenuItem) -> Result<Box<dyn IsMenuItem>, String> {
        Ok(match item {
            NativeMenuItem::Label(label) => Box::new(MenuItem::new(label, false, None)),
            NativeMenuItem::Separator => Box::new(PredefinedMenuItem::separator()),
            NativeMenuItem::Action {
                id, label, enabled, ..
            } => Box::new(MenuItem::with_id(id.as_str(), label, *enabled, None)),
            NativeMenuItem::Check {
                id,
                label,
                checked,
                enabled,
                ..
            } => Box::new(CheckMenuItem::with_id(
                id.as_str(),
                label,
                *enabled,
                *checked,
                None,
            )),
            NativeMenuItem::Submenu {
                label,
                enabled,
                items,
            } => {
                let submenu = Submenu::new(label, *enabled);
                for child in items {
                    let built = build_item(child)?;
                    submenu
                        .append(built.as_ref())
                        .map_err(|error| format!("failed to build tray submenu: {error}"))?;
                }
                Box::new(submenu)
            }
        })
    }

    fn tooltip(model: &TrayMenuModel) -> String {
        let value = format!("MULTICORE — {}", model.status);
        value.chars().take(120).collect()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        #[ignore = "requires isolated D-Bus fixtures from test-linux-shell.sh"]
        fn notifier_availability_matches_registered_host() {
            assert_eq!(
                std::env::var("MULTICORE_ISOLATED_SHELL_TEST").as_deref(),
                Ok("1")
            );
            let expected = std::env::var("MULTICORE_TEST_HOST_PRESENT").unwrap() == "true";
            assert_eq!(super::status_notifier_watcher_running(), expected);
        }

        #[test]
        #[ignore = "requires a desktop session with a system tray host"]
        fn host_session_exposes_a_tray_host() {
            gtk::init().unwrap();
            assert!(super::xembed_tray_running() || super::status_notifier_watcher_running());
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::start;
