//! Ctrl+C/V/X/A/Z/Y on a non-Latin keyboard layout.
//!
//! Slint matches editing shortcuts against the text a key produces, so on the Russian layout
//! Ctrl+C arrives as Ctrl+«с» and copy, paste, cut, select-all, undo and redo silently do
//! nothing. Native Windows and GTK controls match the physical key instead; this filter does
//! the same by re-dispatching such a key press with its Latin letter.
//!
//! The same filter (a window has only one) reports keyboard focus changes of the window.

use std::cell::Cell;
use std::rc::Rc;

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};
use slint::winit_030::{EventResult, WinitWindowAccessor};

/// The Latin letter of a physical editing-shortcut key.
fn shortcut_letter(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyC => 'c',
        KeyCode::KeyV => 'v',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        _ => return None,
    })
}

/// The text to dispatch instead of `produced`, or `None` when Slint already gets a Latin
/// letter (Latin layouts) or the key is not a Ctrl shortcut. Alt is excluded: Ctrl+Alt is
/// AltGr on Windows and types characters.
fn remapped_shortcut(
    modifiers: ModifiersState,
    code: KeyCode,
    produced: Option<&str>,
) -> Option<String> {
    if !modifiers.control_key() || modifiers.alt_key() || modifiers.super_key() {
        return None;
    }
    let letter = shortcut_letter(code)?;
    if produced.is_some_and(|text| text.eq_ignore_ascii_case(letter.encode_utf8(&mut [0; 4]))) {
        return None;
    }
    Some(if modifiers.shift_key() {
        letter.to_ascii_uppercase().to_string()
    } else {
        letter.to_string()
    })
}

/// Installs the filter on `window`. It uses the window's only winit event-filter slot, so it
/// also calls `focus_changed` whenever the window gains or loses keyboard focus.
pub(crate) fn install(window: &slint::Window, focus_changed: impl Fn(bool) + 'static) {
    let modifiers = Rc::new(Cell::new(ModifiersState::empty()));
    window.on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::Focused(focused) => focus_changed(*focused),
            WindowEvent::ModifiersChanged(state) => modifiers.set(state.state()),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return EventResult::Propagate;
                };
                let produced = match &event.logical_key {
                    Key::Character(text) => Some(text.as_str()),
                    _ => None,
                };
                if let Some(text) = remapped_shortcut(modifiers.get(), code, produced) {
                    let text: slint::SharedString = text.into();
                    let event = match event.state {
                        ElementState::Pressed => slint::platform::WindowEvent::KeyPressed { text },
                        ElementState::Released => {
                            slint::platform::WindowEvent::KeyReleased { text }
                        }
                    };
                    window.dispatch_event(event);
                    return EventResult::PreventDefault;
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: ModifiersState = ModifiersState::CONTROL;

    #[test]
    fn russian_layout_ctrl_letters_become_latin_shortcuts() {
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyC, Some("с")),
            Some("c".into())
        );
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyV, Some("м")),
            Some("v".into())
        );
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyX, Some("ч")),
            Some("x".into())
        );
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyA, Some("ф")),
            Some("a".into())
        );
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyZ, Some("я")),
            Some("z".into())
        );
        assert_eq!(
            remapped_shortcut(CTRL | ModifiersState::SHIFT, KeyCode::KeyZ, Some("Я")),
            Some("Z".into())
        );
        // Windows can report the control character instead of a letter.
        assert_eq!(
            remapped_shortcut(CTRL, KeyCode::KeyC, None),
            Some("c".into())
        );
    }

    #[test]
    fn latin_layouts_and_other_keys_are_left_to_slint() {
        assert_eq!(remapped_shortcut(CTRL, KeyCode::KeyC, Some("c")), None);
        assert_eq!(remapped_shortcut(CTRL, KeyCode::KeyV, Some("V")), None);
        assert_eq!(remapped_shortcut(CTRL, KeyCode::KeyB, Some("и")), None);
        assert_eq!(
            remapped_shortcut(ModifiersState::empty(), KeyCode::KeyC, Some("с")),
            None
        );
        // Ctrl+Alt is AltGr on Windows: it types, it is not a shortcut.
        assert_eq!(
            remapped_shortcut(CTRL | ModifiersState::ALT, KeyCode::KeyC, Some("с")),
            None
        );
    }
}
