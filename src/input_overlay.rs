use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::Result;
#[cfg(test)]
use rdev::EventType;
use rdev::{Button, Key};

use crate::cli::{OverlayPosition, OverlayUiArgs};
use crate::protocol::{KeyAction, ModifierFlags, WireEvent};

const MAX_VISIBLE_KEYS: usize = 6;

pub(crate) struct InputOverlay {
    enabled: bool,
    state: OverlayState,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl InputOverlay {
    pub(crate) fn start(enabled: bool, position: OverlayPosition, idle_ms: u64) -> Self {
        if !enabled {
            return Self {
                enabled: false,
                state: OverlayState::default(),
                child: None,
                stdin: None,
            };
        }

        let mut command = match std::env::current_exe() {
            Ok(path) => Command::new(path),
            Err(err) => {
                tracing::warn!("input overlay disabled: unable to locate executable: {err}");
                return Self::disabled();
            }
        };

        command
            .arg("overlay-ui")
            .arg("--position")
            .arg(overlay_position_value(position))
            .arg("--idle-ms")
            .arg(idle_ms.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        match command.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                if stdin.is_none() {
                    tracing::warn!("input overlay disabled: helper stdin unavailable");
                    let _ = child.kill();
                    let _ = child.wait();
                    return Self::disabled();
                }
                Self {
                    enabled: true,
                    state: OverlayState::default(),
                    child: Some(child),
                    stdin,
                }
            }
            Err(err) => {
                tracing::warn!("input overlay disabled: failed to start helper: {err}");
                Self::disabled()
            }
        }
    }

    pub(crate) fn on_wire_event(&mut self, event: &WireEvent) {
        if !self.enabled {
            return;
        }
        if self.state.apply_wire_event(event) {
            self.send_line(&self.state.render_text(MAX_VISIBLE_KEYS));
        }
    }

    pub(crate) fn clear(&mut self) {
        if !self.enabled {
            return;
        }
        if self.state.clear() {
            self.send_line("HIDE");
        }
    }

    fn send_line(&mut self, line: &str) {
        let Some(stdin) = self.stdin.as_mut() else {
            self.enabled = false;
            return;
        };
        if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
            tracing::warn!("input overlay helper disconnected; disabling overlay");
            self.enabled = false;
            self.stdin = None;
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            state: OverlayState::default(),
            child: None,
            stdin: None,
        }
    }
}

impl Drop for InputOverlay {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Default)]
struct OverlayState {
    pressed_buttons: HashSet<Button>,
    pressed_modifier_keys: HashSet<Key>,
    pressed_keys: HashSet<Key>,
    semantic_keys: HashMap<String, String>,
    semantic_modifiers: Option<ModifierFlags>,
}

impl OverlayState {
    #[cfg(test)]
    fn apply_event(&mut self, event: &EventType) -> bool {
        match event {
            EventType::ButtonPress(button) => self.pressed_buttons.insert(*button),
            EventType::ButtonRelease(button) => self.pressed_buttons.remove(button),
            EventType::KeyPress(key) => {
                if is_modifier_key(*key) {
                    self.pressed_modifier_keys.insert(*key)
                } else {
                    self.pressed_keys.insert(*key)
                }
            }
            EventType::KeyRelease(key) => {
                if is_modifier_key(*key) {
                    self.pressed_modifier_keys.remove(key)
                } else {
                    self.pressed_keys.remove(key)
                }
            }
            _ => false,
        }
    }

    fn apply_wire_event(&mut self, event: &WireEvent) -> bool {
        match event {
            WireEvent::Key {
                action,
                key,
                modifiers,
            } => {
                self.semantic_modifiers = Some(modifiers.clone());
                let identity = wire_key_identity(key.physical_code, &key.logical);
                match action {
                    KeyAction::Down | KeyAction::Repeat => {
                        self.semantic_keys
                            .insert(identity, display_key_label(&key.logical));
                        true
                    }
                    KeyAction::Up => self.semantic_keys.remove(&identity).is_some(),
                }
            }
            WireEvent::ModifierChanged { modifiers } => {
                let changed = self.semantic_modifiers.as_ref() != Some(modifiers);
                self.semantic_modifiers = Some(modifiers.clone());
                changed
            }
            WireEvent::MouseButton { button, pressed } => {
                let button = match *button {
                    1 => Button::Left,
                    2 => Button::Middle,
                    3 => Button::Right,
                    value => Button::Unknown(value),
                };
                if *pressed {
                    self.pressed_buttons.insert(button)
                } else {
                    self.pressed_buttons.remove(&button)
                }
            }
            WireEvent::RelativeMotion { .. } | WireEvent::Wheel { .. } => false,
        }
    }

    fn clear(&mut self) -> bool {
        let changed = !(self.pressed_buttons.is_empty()
            && self.pressed_modifier_keys.is_empty()
            && self.pressed_keys.is_empty()
            && self.semantic_keys.is_empty()
            && self.semantic_modifiers.is_none());
        self.pressed_buttons.clear();
        self.pressed_modifier_keys.clear();
        self.pressed_keys.clear();
        self.semantic_keys.clear();
        self.semantic_modifiers = None;
        changed
    }

    fn render_text(&self, max_visible_keys: usize) -> String {
        let mouse = format!(
            "Mouse  {} {} {}",
            chip(self.pressed_buttons.contains(&Button::Left), "LMB"),
            chip(self.pressed_buttons.contains(&Button::Right), "RMB"),
            chip(self.pressed_buttons.contains(&Button::Middle), "MMB")
        );
        let modifiers = format!(
            "Mods   {} {} {} {}",
            chip(self.meta_down(), "CMD"),
            chip(self.ctrl_down(), "CTRL"),
            chip(self.alt_down(), "ALT"),
            chip(self.shift_down(), "SHIFT")
        );

        let mut labels = self
            .pressed_keys
            .iter()
            .filter_map(|key| non_modifier_key_label(*key))
            .collect::<Vec<_>>();
        labels.extend(self.semantic_keys.values().cloned());
        labels.sort_unstable();

        let keys = if labels.is_empty() {
            "Keys   (none)".to_string()
        } else {
            let visible = labels
                .iter()
                .take(max_visible_keys)
                .map(|label| format!("[{}]", label))
                .collect::<Vec<_>>()
                .join(" ");
            let overflow = labels.len().saturating_sub(max_visible_keys);
            if overflow == 0 {
                format!("Keys   {}", visible)
            } else {
                format!("Keys   {} [+{}]", visible, overflow)
            }
        };

        format!("{mouse} | {modifiers} | {keys}")
    }

    fn shift_down(&self) -> bool {
        if let Some(modifiers) = &self.semantic_modifiers {
            return modifiers.left_shift || modifiers.right_shift;
        }
        self.pressed_modifier_keys.contains(&Key::ShiftLeft)
            || self.pressed_modifier_keys.contains(&Key::ShiftRight)
    }

    fn ctrl_down(&self) -> bool {
        if let Some(modifiers) = &self.semantic_modifiers {
            return modifiers.left_control || modifiers.right_control;
        }
        self.pressed_modifier_keys.contains(&Key::ControlLeft)
            || self.pressed_modifier_keys.contains(&Key::ControlRight)
    }

    fn alt_down(&self) -> bool {
        if let Some(modifiers) = &self.semantic_modifiers {
            return modifiers.left_alt || modifiers.right_alt;
        }
        self.pressed_modifier_keys.contains(&Key::Alt)
            || self.pressed_modifier_keys.contains(&Key::AltGr)
    }

    fn meta_down(&self) -> bool {
        if let Some(modifiers) = &self.semantic_modifiers {
            return modifiers.left_meta || modifiers.right_meta;
        }
        self.pressed_modifier_keys.contains(&Key::MetaLeft)
            || self.pressed_modifier_keys.contains(&Key::MetaRight)
    }
}

fn wire_key_identity(physical_code: Option<u16>, logical: &str) -> String {
    physical_code
        .map(|code| format!("physical:{code}"))
        .unwrap_or_else(|| format!("logical:{logical}"))
}

fn display_key_label(logical: &str) -> String {
    let logical = logical.strip_prefix("Key::").unwrap_or(logical);
    match logical {
        "Return" => "ENTER".to_string(),
        "Space" => "SPACE".to_string(),
        "Tab" => "TAB".to_string(),
        "Escape" => "ESC".to_string(),
        "Backspace" => "BACKSPACE".to_string(),
        "Delete" => "DELETE".to_string(),
        _ => logical.to_ascii_uppercase(),
    }
}

fn chip(active: bool, label: &str) -> String {
    if active {
        format!("[{label}]")
    } else {
        format!(" {label} ")
    }
}

fn overlay_position_value(position: OverlayPosition) -> &'static str {
    match position {
        OverlayPosition::TopRight => "top-right",
        OverlayPosition::TopLeft => "top-left",
        OverlayPosition::BottomRight => "bottom-right",
        OverlayPosition::BottomLeft => "bottom-left",
    }
}

fn is_modifier_key(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::Alt
            | Key::AltGr
            | Key::MetaLeft
            | Key::MetaRight
    )
}

fn non_modifier_key_label(key: Key) -> Option<String> {
    if is_modifier_key(key) {
        return None;
    }

    let raw = format!("{:?}", key);
    if raw.starts_with("Unknown(") {
        return None;
    }
    if let Some(letter) = raw.strip_prefix("Key")
        && letter.len() == 1
    {
        return Some(letter.to_string());
    }
    if let Some(number) = raw.strip_prefix("Num")
        && number.len() == 1
    {
        return Some(number.to_string());
    }

    match raw.as_str() {
        "Space" => Some("SPACE".to_string()),
        "Return" => Some("ENTER".to_string()),
        "Tab" => Some("TAB".to_string()),
        "Escape" => Some("ESC".to_string()),
        "Backspace" => Some("BACKSPACE".to_string()),
        "Delete" => Some("DELETE".to_string()),
        "UpArrow" => Some("UP".to_string()),
        "DownArrow" => Some("DOWN".to_string()),
        "LeftArrow" => Some("LEFT".to_string()),
        "RightArrow" => Some("RIGHT".to_string()),
        other => Some(other.to_ascii_uppercase()),
    }
}

#[cfg(target_os = "macos")]
mod ui {
    use anyhow::{Context, Result};
    use cocoa::appkit::{NSApp, NSApplication, NSApplicationActivationPolicyAccessory};
    use cocoa::base::{NO, YES, id, nil};
    use cocoa::foundation::{
        NSAutoreleasePool, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString,
    };
    use objc::{class, msg_send, sel, sel_impl};
    use std::io::{self, BufRead};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::cli::{OverlayPosition, OverlayUiArgs};

    pub(crate) fn run(args: OverlayUiArgs) -> Result<()> {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let app = NSApp();
            app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);

            let backend = OverlayBackend::new(args.position)
                .context("failed to create overlay window on main thread")?;
            let idle = Duration::from_millis(args.idle_ms.max(1));
            let mut visible = false;
            let mut last_activity = Instant::now();

            let (line_tx, line_rx) = mpsc::channel::<String>();
            thread::spawn(move || {
                let stdin = io::stdin();
                for line in stdin.lock().lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
            });

            loop {
                match line_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => {
                        if line == "HIDE" {
                            backend.hide();
                            visible = false;
                        } else if !line.trim().is_empty() {
                            backend.set_text(&line);
                            backend.show();
                            visible = true;
                            last_activity = Instant::now();
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                if visible && last_activity.elapsed() >= idle {
                    backend.hide();
                    visible = false;
                }

                pump_app_events(app);
            }

            backend.close();
            Ok(())
        }
    }

    unsafe fn pump_app_events(app: id) {
        let date = unsafe { NSDate::distantPast(nil) };
        let mode = unsafe { NSDefaultRunLoopMode };
        let event: id = msg_send![
            app,
            nextEventMatchingMask:u64::MAX
            untilDate:date
            inMode:mode
            dequeue:YES
        ];
        if event != nil {
            let _: () = msg_send![app, sendEvent:event];
        }
        let _: () = msg_send![app, updateWindows];
    }

    struct OverlayBackend {
        window: id,
        label: id,
    }

    impl OverlayBackend {
        unsafe fn new(position: OverlayPosition) -> Option<Self> {
            let screen: id = msg_send![class!(NSScreen), mainScreen];
            if screen == nil {
                return None;
            }
            let frame: NSRect = msg_send![screen, visibleFrame];
            let width = 760.0;
            let height = 40.0;
            let margin = 24.0;

            let origin_x = match position {
                OverlayPosition::TopRight | OverlayPosition::BottomRight => {
                    frame.origin.x + frame.size.width - width - margin
                }
                OverlayPosition::TopLeft | OverlayPosition::BottomLeft => frame.origin.x + margin,
            };
            let origin_y = match position {
                OverlayPosition::TopRight | OverlayPosition::TopLeft => {
                    frame.origin.y + frame.size.height - height - margin
                }
                OverlayPosition::BottomRight | OverlayPosition::BottomLeft => {
                    frame.origin.y + margin
                }
            };

            let rect = NSRect::new(NSPoint::new(origin_x, origin_y), NSSize::new(width, height));
            let window: id = msg_send![class!(NSWindow), alloc];
            let window: id = msg_send![
                window,
                initWithContentRect:rect
                styleMask:0u64
                backing:2u64
                defer:NO
            ];
            if window == nil {
                return None;
            }
            let _: () = msg_send![window, setReleasedWhenClosed:NO];
            let _: () = msg_send![window, setOpaque:NO];
            let clear_color: id =
                msg_send![class!(NSColor), colorWithCalibratedWhite:0.0f64 alpha:0.68f64];
            let _: () = msg_send![window, setBackgroundColor:clear_color];
            let _: () = msg_send![window, setHasShadow:YES];
            let _: () = msg_send![window, setIgnoresMouseEvents:YES];
            let _: () = msg_send![window, setCollectionBehavior:1u64];
            let _: () = msg_send![window, setLevel:3i64];

            let content_view: id = msg_send![window, contentView];
            let label: id = msg_send![class!(NSTextField), alloc];
            let label: id = msg_send![
                label,
                initWithFrame:NSRect::new(
                    NSPoint::new(12.0, 8.0),
                    NSSize::new(width - 24.0, height - 16.0),
                )
            ];
            if label == nil {
                return None;
            }
            let _: () = msg_send![label, setEditable:NO];
            let _: () = msg_send![label, setSelectable:NO];
            let _: () = msg_send![label, setBezeled:NO];
            let _: () = msg_send![label, setBordered:NO];
            let _: () = msg_send![label, setDrawsBackground:NO];
            let white: id =
                msg_send![class!(NSColor), colorWithCalibratedWhite:0.96f64 alpha:0.99f64];
            let _: () = msg_send![label, setTextColor:white];
            let font: id =
                msg_send![class!(NSFont), monospacedSystemFontOfSize:12.5f64 weight:0.42f64];
            let _: () = msg_send![label, setFont:font];
            let _: () = msg_send![label, setLineBreakMode:4u64];
            let _: () = msg_send![content_view, addSubview:label];

            let _: () = msg_send![window, setAlphaValue:0.0f64];
            let _: () = msg_send![window, orderFrontRegardless];

            Some(Self { window, label })
        }

        unsafe fn set_text(&self, text: &str) {
            let text = unsafe { NSString::alloc(nil).init_str(text) };
            let _: () = msg_send![self.label, setStringValue:text];
        }

        unsafe fn show(&self) {
            let _: () = msg_send![self.window, setAlphaValue:1.0f64];
            let _: () = msg_send![self.window, orderFrontRegardless];
        }

        unsafe fn hide(&self) {
            let _: () = msg_send![self.window, setAlphaValue:0.0f64];
        }

        unsafe fn close(&self) {
            let _: () = msg_send![self.window, orderOut:nil];
            let _: () = msg_send![self.window, close];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod ui {
    use anyhow::{Result, bail};

    use crate::cli::OverlayUiArgs;

    pub(crate) fn run(_args: OverlayUiArgs) -> Result<()> {
        bail!("overlay-ui is supported on macOS only")
    }
}

pub(crate) fn run_overlay_ui(args: OverlayUiArgs) -> Result<()> {
    ui::run(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_state_render_shows_pressed_inputs() {
        let mut state = OverlayState::default();
        assert!(state.apply_event(&EventType::ButtonPress(Button::Left)));
        assert!(state.apply_event(&EventType::KeyPress(Key::MetaLeft)));
        assert!(state.apply_event(&EventType::KeyPress(Key::KeyV)));

        let rendered = state.render_text(6);
        assert!(rendered.contains("[LMB]"));
        assert!(rendered.contains("[CMD]"));
        assert!(rendered.contains("[V]"));
    }

    #[test]
    fn overlay_state_render_caps_visible_keys() {
        let mut state = OverlayState::default();
        for key in [
            Key::KeyA,
            Key::KeyB,
            Key::KeyC,
            Key::KeyD,
            Key::KeyE,
            Key::KeyF,
            Key::KeyG,
        ] {
            assert!(state.apply_event(&EventType::KeyPress(key)));
        }
        let rendered = state.render_text(3);
        assert!(rendered.contains("[+4]"));
    }

    #[test]
    fn overlay_state_clear_resets_all_inputs() {
        let mut state = OverlayState::default();
        assert!(state.apply_event(&EventType::ButtonPress(Button::Right)));
        assert!(state.apply_event(&EventType::KeyPress(Key::ControlLeft)));
        assert!(state.apply_event(&EventType::KeyPress(Key::KeyX)));

        assert!(state.clear());
        assert!(!state.clear());

        let rendered = state.render_text(6);
        assert!(rendered.contains("Keys   (none)"));
        assert!(!rendered.contains("[CTRL]"));
        assert!(!rendered.contains("[RMB]"));
    }

    #[test]
    fn overlay_state_renders_semantic_keys_and_modifiers() {
        let mut state = OverlayState::default();
        let modifiers = ModifierFlags {
            left_shift: true,
            ..ModifierFlags::default()
        };
        assert!(state.apply_wire_event(&WireEvent::Key {
            action: KeyAction::Down,
            key: crate::protocol::WireKey {
                physical_code: Some(0),
                logical: "a".to_string(),
            },
            modifiers: modifiers.clone(),
        }));

        let rendered = state.render_text(6);
        assert!(rendered.contains("[SHIFT]"));
        assert!(rendered.contains("[A]"));

        assert!(state.apply_wire_event(&WireEvent::Key {
            action: KeyAction::Up,
            key: crate::protocol::WireKey {
                physical_code: Some(0),
                logical: "KeyA".to_string(),
            },
            modifiers: ModifierFlags::default(),
        }));
        assert!(!state.render_text(6).contains("[A]"));
    }
}
