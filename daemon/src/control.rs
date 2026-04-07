// control.rs — mouse/keyboard input execution, parse_key, parse_button

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse};
use serde::Deserialize;
use std::sync::Mutex;
use tracing::warn;

use crate::capture::ResizeState;

#[derive(Deserialize, Debug)]
pub struct ControlCmd {
    pub action:            String,
    #[serde(default)] pub x:       i32,
    #[serde(default)] pub y:       i32,
    #[serde(default)] pub button:  String,
    #[serde(default)] pub key:     String,
    #[serde(default)] pub delta_y: i32,
    #[serde(default)] pub width:   Option<u32>,
    #[serde(default)] pub height:  Option<u32>,
}

pub fn execute_control(
    cmd:       ControlCmd,
    resize_state: &ResizeState,
    enigo:     &Mutex<Enigo>,
) {
    if cmd.action == "resize" {
        resize_state.set_target(cmd.width, cmd.height);
        return;
    }

    let mut enigo = match enigo.lock() {
        Ok(e) => e,
        Err(_) => {
            warn!("Enigo lock poisoned");
            return;
        }
    };

    match cmd.action.as_str() {
        "mousemove" => { let _ = enigo.move_mouse(cmd.x, cmd.y, Coordinate::Abs); }
        "mousedown" => { let _ = enigo.button(parse_button(&cmd.button), Direction::Press); }
        "mouseup"   => { let _ = enigo.button(parse_button(&cmd.button), Direction::Release); }
        "click"     => { let _ = enigo.button(parse_button(&cmd.button), Direction::Click); }
        "scroll"    => { let _ = enigo.scroll(cmd.delta_y, Axis::Vertical); }
        "keydown"   => { if let Some(k) = parse_key(&cmd.key) { let _ = enigo.key(k, Direction::Press); } }
        "keyup"     => { if let Some(k) = parse_key(&cmd.key) { let _ = enigo.key(k, Direction::Release); } }
        other       => warn!("Unknown control action: {}", other),
    }
}

pub fn parse_button(s: &str) -> Button {
    match s.to_lowercase().as_str() {
        "right"  => Button::Right,
        "middle" => Button::Middle,
        _        => Button::Left,
    }
}

pub fn parse_key(s: &str) -> Option<Key> {
    match s.to_lowercase().as_str() {
        "enter" | "return" => Some(Key::Return),
        "escape" | "esc"   => Some(Key::Escape),
        "backspace"        => Some(Key::Backspace),
        "tab"              => Some(Key::Tab),
        "delete" | "del"   => Some(Key::Delete),
        "arrowleft" | "left" => Some(Key::LeftArrow),
        "arrowright" | "right" => Some(Key::RightArrow),
        "arrowup" | "up"     => Some(Key::UpArrow),
        "arrowdown" | "down" => Some(Key::DownArrow),
        "home"             => Some(Key::Home),
        "end"              => Some(Key::End),
        "pageup"           => Some(Key::PageUp),
        "pagedown"         => Some(Key::PageDown),
        "f1" => Some(Key::F1), "f2" => Some(Key::F2), "f3" => Some(Key::F3), "f4" => Some(Key::F4),
        "f5" => Some(Key::F5), "f6" => Some(Key::F6), "f7" => Some(Key::F7), "f8" => Some(Key::F8),
        "f9" => Some(Key::F9), "f10" => Some(Key::F10), "f11" => Some(Key::F11), "f12" => Some(Key::F12),
        "control" | "ctrl" => Some(Key::Control),
        "alt"              => Some(Key::Alt),
        "shift"            => Some(Key::Shift),
        "meta" | "super"   => Some(Key::Meta),
        _ => {
            let mut chars = s.chars();
            if let Some(ch) = chars.next() {
                if chars.next().is_none() {
                    return Some(Key::Unicode(ch));
                }
            }
            None
        }
    }
}
