use anyhow::{Result, anyhow};
use rdev::{Button, EventType, Key};

use crate::protocol::ModifierFlags;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::display::{display_layout, pointer_location};
    use core_graphics::{
        event::{
            CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit,
        },
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    pub(crate) fn prepare_event_for_injection(event: &EventType) -> Result<EventType> {
        let EventType::MouseMove { x, y } = event else {
            return Ok(*event);
        };
        let (fallback_x, fallback_y) = pointer_location()?;
        let (x, y) = display_layout()?
            .clamp_absolute_point(*x, *y, fallback_x, fallback_y)
            .ok_or_else(|| anyhow!("macOS display layout is empty"))?;
        Ok(EventType::MouseMove { x, y })
    }

    pub(crate) fn inject_event(event: &EventType) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create macOS HID event source"))?;

        match event {
            EventType::KeyPress(key) | EventType::KeyRelease(key) => {
                let keycode =
                    keycode(*key).ok_or_else(|| anyhow!("unsupported macOS key: {key:?}"))?;
                let down = matches!(event, EventType::KeyPress(_));
                let event = CGEvent::new_keyboard_event(source, keycode, down)
                    .map_err(|_| anyhow!("failed to create macOS keyboard event"))?;
                event.post(CGEventTapLocation::HID);
            }
            EventType::ButtonPress(button) | EventType::ButtonRelease(button) => {
                let (event_type, mouse_button) = mouse_button_event(*button, event)?;
                let location = bounded_pointer_location()?;
                let event = CGEvent::new_mouse_event(source, event_type, location, mouse_button)
                    .map_err(|_| anyhow!("failed to create macOS mouse event"))?;
                event.post(CGEventTapLocation::HID);
            }
            EventType::MouseMove { x, y } => {
                let event = CGEvent::new_mouse_event(
                    source,
                    CGEventType::MouseMoved,
                    CGPoint::new(*x, *y),
                    CGMouseButton::Left,
                )
                .map_err(|_| anyhow!("failed to create macOS motion event"))?;
                event.post(CGEventTapLocation::HID);
            }
            EventType::Wheel { delta_x, delta_y } => {
                let event = CGEvent::new_scroll_event(
                    source,
                    ScrollEventUnit::PIXEL,
                    2,
                    *delta_y as i32,
                    *delta_x as i32,
                    0,
                )
                .map_err(|_| anyhow!("failed to create macOS scroll event"))?;
                event.post(CGEventTapLocation::HID);
            }
        }
        Ok(())
    }

    pub(crate) fn inject_prepared_event(event: &EventType) -> Result<()> {
        inject_event(event)
    }

    pub(crate) fn cancel_input_composition() -> Result<()> {
        if !crate::macos_keyboard::current_input_source_is_non_english() {
            return Ok(());
        }
        inject_event(&EventType::KeyPress(Key::Escape))?;
        inject_event(&EventType::KeyRelease(Key::Escape))
    }

    pub(crate) fn keycode_for_key(key: Key) -> Option<u16> {
        keycode(key)
    }

    pub(crate) fn inject_key_with_modifiers(
        keycode: u16,
        down: bool,
        repeat: bool,
        modifiers: &ModifierFlags,
    ) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create macOS HID event source"))?;
        let event = CGEvent::new_keyboard_event(source, keycode, down)
            .map_err(|_| anyhow!("failed to create macOS keyboard event"))?;
        event.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, i64::from(repeat));
        event.set_flags(flags_for_modifiers(modifiers));
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub(crate) fn inject_paste() -> Result<()> {
        let modifiers = ModifierFlags {
            left_meta: true,
            ..ModifierFlags::default()
        };
        inject_key_with_modifiers(
            keycode(Key::KeyV).ok_or_else(|| anyhow!("missing V keycode"))?,
            true,
            false,
            &modifiers,
        )?;
        inject_key_with_modifiers(
            keycode(Key::KeyV).ok_or_else(|| anyhow!("missing V keycode"))?,
            false,
            false,
            &modifiers,
        )
    }

    pub(crate) fn inject_relative_move_with_button(
        dx: i32,
        dy: i32,
        button: Option<Button>,
    ) -> Result<()> {
        let location = current_location()?;
        let layout = display_layout()?;
        let (target_x, target_y, actual_dx, actual_dy) = layout
            .clamp_pointer_move(location.x, location.y, dx, dy)
            .ok_or_else(|| anyhow!("macOS display layout is empty"))?;
        let event_type = match button {
            Some(Button::Left) => CGEventType::LeftMouseDragged,
            Some(Button::Right) => CGEventType::RightMouseDragged,
            Some(Button::Middle) => CGEventType::OtherMouseDragged,
            Some(Button::Unknown(_)) | None => CGEventType::MouseMoved,
        };
        let mouse_button = match button {
            Some(Button::Right) => CGMouseButton::Right,
            Some(Button::Middle) => CGMouseButton::Center,
            _ => CGMouseButton::Left,
        };
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create macOS HID event source"))?;
        let event = CGEvent::new_mouse_event(
            source,
            event_type,
            CGPoint::new(target_x, target_y),
            mouse_button,
        )
        .map_err(|_| anyhow!("failed to create macOS drag event"))?;
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, i64::from(actual_dx));
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, i64::from(actual_dy));
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn current_location() -> Result<CGPoint> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create macOS HID event source"))?;
        Ok(CGEvent::new(source)
            .map_err(|_| anyhow!("failed to read current macOS pointer location"))?
            .location())
    }

    fn bounded_pointer_location() -> Result<CGPoint> {
        let location = current_location()?;
        let layout = display_layout()?;
        let (x, y) = layout
            .clamp_absolute_point(location.x, location.y, location.x, location.y)
            .ok_or_else(|| anyhow!("macOS display layout is empty"))?;
        Ok(CGPoint::new(x, y))
    }

    fn flags_for_modifiers(modifiers: &ModifierFlags) -> core_graphics::event::CGEventFlags {
        use core_graphics::event::CGEventFlags;

        let mut flags = CGEventFlags::empty();
        if modifiers.left_shift || modifiers.right_shift {
            flags |= CGEventFlags::CGEventFlagShift;
        }
        if modifiers.left_control || modifiers.right_control {
            flags |= CGEventFlags::CGEventFlagControl;
        }
        if modifiers.left_alt || modifiers.right_alt {
            flags |= CGEventFlags::CGEventFlagAlternate;
        }
        if modifiers.left_meta || modifiers.right_meta {
            flags |= CGEventFlags::CGEventFlagCommand;
        }
        flags
    }

    fn mouse_button_event(
        button: Button,
        event: &EventType,
    ) -> Result<(CGEventType, CGMouseButton)> {
        let down = matches!(event, EventType::ButtonPress(_));
        let mapped = match button {
            Button::Left => (
                if down {
                    CGEventType::LeftMouseDown
                } else {
                    CGEventType::LeftMouseUp
                },
                CGMouseButton::Left,
            ),
            Button::Right => (
                if down {
                    CGEventType::RightMouseDown
                } else {
                    CGEventType::RightMouseUp
                },
                CGMouseButton::Right,
            ),
            Button::Middle => (
                if down {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
            ),
            Button::Unknown(_) => {
                return Err(anyhow!("unsupported macOS mouse button: {button:?}"));
            }
        };
        Ok(mapped)
    }

    fn keycode(key: Key) -> Option<u16> {
        Some(match key {
            Key::KeyA => 0,
            Key::KeyS => 1,
            Key::KeyD => 2,
            Key::KeyF => 3,
            Key::KeyH => 4,
            Key::KeyG => 5,
            Key::KeyZ => 6,
            Key::KeyX => 7,
            Key::KeyC => 8,
            Key::KeyV => 9,
            Key::KeyB => 11,
            Key::KeyQ => 12,
            Key::KeyW => 13,
            Key::KeyE => 14,
            Key::KeyR => 15,
            Key::KeyY => 16,
            Key::KeyT => 17,
            Key::KeyO => 31,
            Key::KeyU => 32,
            Key::KeyI => 34,
            Key::KeyP => 35,
            Key::KeyL => 37,
            Key::KeyJ => 38,
            Key::KeyK => 40,
            Key::KeyN => 45,
            Key::KeyM => 46,
            Key::Num1 => 18,
            Key::Num2 => 19,
            Key::Num3 => 20,
            Key::Num4 => 21,
            Key::Num6 => 22,
            Key::Num5 => 23,
            Key::Equal => 24,
            Key::Num9 => 25,
            Key::Num7 => 26,
            Key::Minus => 27,
            Key::Num8 => 28,
            Key::Num0 => 29,
            Key::RightBracket => 30,
            Key::LeftBracket => 33,
            Key::Quote => 39,
            Key::SemiColon => 41,
            Key::Comma => 43,
            Key::Slash => 44,
            Key::Dot => 47,
            Key::BackSlash => 42,
            Key::BackQuote => 50,
            Key::Return => 36,
            Key::Tab => 48,
            Key::Space => 49,
            Key::Backspace => 51,
            Key::Escape => 53,
            Key::Delete => 117,
            Key::MetaLeft => 55,
            Key::MetaRight => 54,
            Key::ShiftLeft => 56,
            Key::ShiftRight => 60,
            Key::CapsLock => 57,
            Key::Alt => 58,
            Key::AltGr => 61,
            Key::ControlLeft => 59,
            Key::ControlRight => 62,
            Key::F1 => 122,
            Key::F2 => 120,
            Key::F3 => 99,
            Key::F4 => 118,
            Key::F5 => 96,
            Key::F6 => 97,
            Key::F7 => 98,
            Key::F8 => 100,
            Key::F9 => 101,
            Key::F10 => 109,
            Key::F11 => 103,
            Key::F12 => 111,
            Key::Home => 115,
            Key::End => 119,
            Key::PageUp => 116,
            Key::PageDown => 121,
            Key::LeftArrow => 123,
            Key::RightArrow => 124,
            Key::DownArrow => 125,
            Key::UpArrow => 126,
            Key::PrintScreen => 105,
            Key::ScrollLock => 107,
            Key::Pause => 113,
            Key::NumLock => 71,
            Key::Insert => 114,
            Key::KpReturn => 76,
            Key::KpMinus => 78,
            Key::KpPlus => 69,
            Key::KpMultiply => 67,
            Key::KpDivide => 75,
            Key::Kp0 => 82,
            Key::Kp1 => 83,
            Key::Kp2 => 84,
            Key::Kp3 => 85,
            Key::Kp4 => 86,
            Key::Kp5 => 87,
            Key::Kp6 => 88,
            Key::Kp7 => 89,
            Key::Kp8 => 91,
            Key::Kp9 => 92,
            Key::KpDelete => 65,
            Key::Unknown(code) if code <= u16::MAX as u32 => code as u16,
            _ => return None,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub(crate) fn inject_event(_event: &EventType) -> Result<()> {
        Err(anyhow!(
            "native macOS injection is unavailable on this platform"
        ))
    }

    pub(crate) fn inject_prepared_event(event: &EventType) -> Result<()> {
        inject_event(event)
    }

    pub(crate) fn prepare_event_for_injection(event: &EventType) -> Result<EventType> {
        Ok(event.clone())
    }

    pub(crate) fn cancel_input_composition() -> Result<()> {
        Ok(())
    }

    pub(crate) fn keycode_for_key(_key: Key) -> Option<u16> {
        None
    }

    pub(crate) fn inject_paste() -> Result<()> {
        Err(anyhow!("paste injection is only supported on macOS"))
    }

    pub(crate) fn inject_key_with_modifiers(
        _keycode: u16,
        _down: bool,
        _repeat: bool,
        _modifiers: &ModifierFlags,
    ) -> Result<()> {
        Err(anyhow!(
            "native macOS injection is unavailable on this platform"
        ))
    }

    pub(crate) fn inject_relative_move_with_button(
        _dx: i32,
        _dy: i32,
        _button: Option<Button>,
    ) -> Result<()> {
        Err(anyhow!(
            "native macOS injection is unavailable on this platform"
        ))
    }
}

pub(crate) fn inject_event(event: &EventType) -> Result<()> {
    let prepared = imp::prepare_event_for_injection(event)?;
    imp::inject_event(&prepared)
}

pub(crate) fn inject_prepared_event(event: &EventType) -> Result<()> {
    imp::inject_prepared_event(event)
}

pub(crate) fn prepare_event_for_injection(event: &EventType) -> Result<EventType> {
    imp::prepare_event_for_injection(event)
}

pub(crate) fn cancel_input_composition() -> Result<()> {
    imp::cancel_input_composition()
}

pub(crate) fn keycode_for_key(key: Key) -> Option<u16> {
    imp::keycode_for_key(key)
}

pub(crate) fn inject_key_with_modifiers(
    keycode: u16,
    down: bool,
    repeat: bool,
    modifiers: &ModifierFlags,
) -> Result<()> {
    imp::inject_key_with_modifiers(keycode, down, repeat, modifiers)
}

pub(crate) fn inject_paste() -> Result<()> {
    imp::inject_paste()
}

pub(crate) fn inject_relative_move_with_button(
    dx: i32,
    dy: i32,
    button: Option<Button>,
) -> Result<()> {
    imp::inject_relative_move_with_button(dx, dy, button)
}
