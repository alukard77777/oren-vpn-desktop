use std::collections::BTreeSet;

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
    pub(crate) selected_node_label: String,
    pub(crate) nodes: Vec<TrayNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayNode {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) icon: String,
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayInput {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) groups: Vec<TrayGroup>,
    pub(crate) route_selection_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuEntry {
    pub(crate) icon: String,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) checked: bool,
    pub(crate) enabled: bool,
    pub(crate) command: TrayCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuModel {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) groups: Vec<TrayMenuEntry>,
    pub(crate) nodes: Vec<TrayMenuEntry>,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrayAccordionState {
    expanded_group_ids: BTreeSet<String>,
}

impl TrayAccordionState {
    pub(crate) fn toggle(&mut self, group_id: &str) -> bool {
        if self.expanded_group_ids.remove(group_id) {
            false
        } else {
            self.expanded_group_ids.insert(group_id.to_owned());
            true
        }
    }

    pub(crate) fn is_expanded(&self, group_id: &str) -> bool {
        self.expanded_group_ids.contains(group_id)
    }

    pub(crate) fn retain_available<'a>(&mut self, group_ids: impl Iterator<Item = &'a str>) {
        let available = group_ids.collect::<BTreeSet<_>>();
        self.expanded_group_ids
            .retain(|group_id| available.contains(group_id.as_str()));
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
                label: node.label,
                detail: String::new(),
                checked: node.selected,
                enabled: input.route_selection_enabled,
                command: TrayCommand::SelectNode {
                    group_id: group_id.clone(),
                    node_id: node.id,
                },
            }));
            TrayMenuEntry {
                icon: group.icon,
                label: group.label,
                detail: group.selected_node_label,
                checked: false,
                enabled: input.route_selection_enabled,
                command: TrayCommand::SelectGroup { group_id },
            }
        })
        .collect();

    TrayMenuModel {
        status: input.status,
        connection_label: input.connection_label,
        connection_enabled: input.connection_enabled,
        groups,
        nodes,
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
            let icon = Icon::from_rgba(crate::app_icon::rgba(32), 32, 32)
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
        let value = format!("MultiCore — {}", model.status);
        value.chars().take(120).collect()
    }
}

#[cfg(windows)]
pub(crate) use native::start;

#[cfg(not(windows))]
pub(crate) struct TrayRuntime;

#[cfg(not(windows))]
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
        TrayAccordionState, TrayCommand, TrayGroup, TrayInput, TrayInteraction, TrayNode,
        TrayPointerButton, TrayPointerState, TrayPopupModelCache, build_menu_model,
        pointer_interaction,
    };

    #[test]
    fn menu_exposes_every_groups_nodes_and_icons_without_treating_groups_as_alternatives() {
        let input = TrayInput {
            status: "В сети".into(),
            connection_label: "Отключиться".into(),
            connection_enabled: true,
            groups: vec![
                TrayGroup {
                    id: "server:opaque".into(),
                    label: "Сервер".into(),
                    icon: "🌍".into(),
                    selected_node_label: "Швеция".into(),
                    nodes: vec![TrayNode {
                        id: "node:a".into(),
                        label: "Швеция".into(),
                        icon: "🇸🇪".into(),
                        selected: true,
                    }],
                },
                TrayGroup {
                    id: "games:opaque".into(),
                    label: "Игры".into(),
                    icon: "🎮".into(),
                    selected_node_label: "Direct".into(),
                    nodes: vec![TrayNode {
                        id: "node:b".into(),
                        label: "Direct".into(),
                        icon: "↗".into(),
                        selected: true,
                    }],
                },
            ],
            route_selection_enabled: true,
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
            groups: Vec::new(),
            route_selection_enabled: true,
        });

        assert!(model.nodes.is_empty());
        assert!(!model.connection_enabled);
    }

    #[test]
    fn accordion_expands_multiple_route_groups_independently() {
        let mut accordion = TrayAccordionState::default();

        assert!(accordion.toggle("server"));
        assert!(accordion.toggle("games"));
        assert!(accordion.is_expanded("server"));
        assert!(accordion.is_expanded("games"));

        assert!(!accordion.toggle("server"));
        assert!(!accordion.is_expanded("server"));
        assert!(accordion.is_expanded("games"));

        accordion.retain_available(["server"].into_iter());
        assert!(!accordion.is_expanded("games"));
    }

    #[test]
    fn popup_model_cache_suppresses_identical_timer_rebinds() {
        let model = build_menu_model(TrayInput {
            status: "Готово".into(),
            connection_label: "Подключиться".into(),
            connection_enabled: true,
            groups: Vec::new(),
            route_selection_enabled: true,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayPointerButton {
    Left,
    Right,
    Middle,
}

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
