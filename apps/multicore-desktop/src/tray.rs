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
    pub(crate) selected: bool,
    pub(crate) selected_node_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayInput {
    pub(crate) status: String,
    pub(crate) connection_label: String,
    pub(crate) connection_enabled: bool,
    pub(crate) groups: Vec<TrayGroup>,
    pub(crate) selected_group_id: Option<String>,
    pub(crate) nodes: Vec<(String, String, bool)>,
    pub(crate) route_selection_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayMenuEntry {
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

pub(crate) fn build_menu_model(input: TrayInput) -> TrayMenuModel {
    let groups = input
        .groups
        .into_iter()
        .map(|group| TrayMenuEntry {
            label: group.label,
            detail: group.selected_node_label,
            checked: group.selected,
            enabled: input.route_selection_enabled,
            command: TrayCommand::SelectGroup { group_id: group.id },
        })
        .collect();
    let nodes = input
        .selected_group_id
        .map(|group_id| {
            input
                .nodes
                .into_iter()
                .map(|(node_id, label, selected)| TrayMenuEntry {
                    label,
                    detail: String::new(),
                    checked: selected,
                    enabled: input.route_selection_enabled,
                    command: TrayCommand::SelectNode {
                        group_id: group_id.clone(),
                        node_id,
                    },
                })
                .collect()
        })
        .unwrap_or_default();

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
        TrayCommand, TrayGroup, TrayInput, TrayInteraction, TrayPointerButton, TrayPointerState,
        build_menu_model, pointer_interaction,
    };

    #[test]
    fn menu_exposes_contextual_connection_and_current_group_nodes() {
        let input = TrayInput {
            status: "В сети".into(),
            connection_label: "Отключиться".into(),
            connection_enabled: true,
            groups: vec![
                TrayGroup {
                    id: "server:opaque".into(),
                    label: "Сервер".into(),
                    selected: true,
                    selected_node_label: "Швеция".into(),
                },
                TrayGroup {
                    id: "games:opaque".into(),
                    label: "Игры".into(),
                    selected: false,
                    selected_node_label: "Direct".into(),
                },
            ],
            selected_group_id: Some("server:opaque".into()),
            nodes: vec![("node:a".into(), "Швеция".into(), true)],
            route_selection_enabled: true,
        };

        let model = build_menu_model(input);
        assert_eq!(model.status, "В сети");
        assert_eq!(model.connection_label, "Отключиться");
        assert_eq!(model.groups.len(), 2);
        assert_eq!(model.groups[0].detail, "Швеция");
        assert_eq!(model.groups[1].detail, "Direct");
        assert_eq!(model.nodes.len(), 1);
        assert_eq!(
            model.nodes[0].command,
            TrayCommand::SelectNode {
                group_id: "server:opaque".into(),
                node_id: "node:a".into(),
            }
        );
    }

    #[test]
    fn nodes_are_disabled_without_a_current_group() {
        let model = build_menu_model(TrayInput {
            status: "Готово".into(),
            connection_label: "Подключиться".into(),
            connection_enabled: false,
            groups: Vec::new(),
            selected_group_id: None,
            nodes: vec![("stale".into(), "Старый".into(), false)],
            route_selection_enabled: true,
        });

        assert!(model.nodes.is_empty());
        assert!(!model.connection_enabled);
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
