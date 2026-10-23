//! Translate egui events into bytes for the remote pty.
//!
//! egui reports printable characters via `Event::Text`, but control
//! combinations (Ctrl+X, Alt+letter) and special keys (arrows, F-keys)
//! arrive as `Event::Key`. This module implements the xterm-style
//! encoding for those, plus a normaliser for the clipboard events that
//! egui emits instead of raw keys.

use egui::{Key, Modifiers};

/// Normalise an egui event into bytes for the remote pty.
///
/// egui collapses Ctrl+C / Ctrl+X / Ctrl+V into `Event::Copy` / `Cut` /
/// `Paste` before the app sees a `Key` event (egui issue #4065). For a
/// terminal these must be reconstructed as the standard ASCII control
/// bytes, which every xterm-compatible program understands:
///
///   Ctrl+C -> 0x03 (ETX)
///   Ctrl+X -> 0x18 (CAN)
///   Ctrl+V -> 0x16 (SYN)
///
/// This mapping is universal; it is not specific to nano or any other
/// program. Returns `None` for events that should be ignored.
pub fn event_to_bytes(event: &egui::Event, mods: Modifiers) -> Option<Vec<u8>> {
    match event {
        egui::Event::Text(text) => {
            if mods.ctrl && !mods.alt {
                // Ctrl+letter is delivered as Key / Copy / Cut / Paste.
                None
            } else if mods.alt && !mods.ctrl {
                // Alt+char -> ESC prefix (meta key).
                let mut out = Vec::with_capacity(1 + text.len());
                out.push(0x1b);
                out.extend_from_slice(text.as_bytes());
                Some(out)
            } else {
                Some(text.as_bytes().to_vec())
            }
        }
        egui::Event::Paste(text) => {
            // Bare Ctrl+V is reported as Paste. In a terminal it must
            // send SYN, not paste text. Ctrl+Shift+V is the real paste
            // shortcut, and middle-click paste also lands here without
            // Ctrl, so both of those still paste text.
            if mods.ctrl && !mods.shift && !mods.alt {
                Some(vec![0x16])
            } else {
                Some(text.as_bytes().to_vec())
            }
        }
        egui::Event::Copy => Some(vec![0x03]), // Ctrl+C -> ETX
        egui::Event::Cut => Some(vec![0x18]),  // Ctrl+X -> CAN
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            // Bare Ctrl+C/X/V were already turned into Copy/Cut/Paste
            // above; skip the raw Key to avoid sending the byte twice.
            if modifiers.ctrl
                && !modifiers.shift
                && !modifiers.alt
                && matches!(key, egui::Key::C | egui::Key::X | egui::Key::V)
            {
                return None;
            }
            encode_key(*key, *modifiers)
        }
        _ => None,
    }
}

/// Encode a key press that is not a plain printable character.
///
/// Returns `None` for keys whose text is delivered through `Event::Text`
/// (letters, digits, punctuation without Ctrl/Alt).
pub fn encode_key(key: Key, m: Modifiers) -> Option<Vec<u8>> {
    // Ctrl+<key> -> C0 control byte. Alt is excluded because AltGr on
    // Linux is often reported as Ctrl+Alt and should produce text.
    if m.ctrl && !m.alt {
        if let Some(b) = ctrl_byte(key) {
            return Some(vec![b]);
        }
    }

    special_key(key, m)
}

/// Universal ASCII control-code mapping for Ctrl+letter.
///
/// Ctrl+A = 0x01 (SOH) … Ctrl+Z = 0x1A (SUB). This is the same on every
/// Unix terminal; the remote program, not the client, decides what each
/// byte means.
fn ctrl_byte(key: Key) -> Option<u8> {
    use Key::*;
    Some(match key {
        A => 0x01, B => 0x02, C => 0x03, D => 0x04, E => 0x05,
        F => 0x06, G => 0x07, H => 0x08, I => 0x09, J => 0x0a,
        K => 0x0b, L => 0x0c, M => 0x0d, N => 0x0e, O => 0x0f,
        P => 0x10, Q => 0x11, R => 0x12, S => 0x13, T => 0x14,
        U => 0x15, V => 0x16, W => 0x17, X => 0x18, Y => 0x19,
        Z => 0x1a,
        Space => 0x00,
        OpenBracket => 0x1b,   // Ctrl+[ == ESC
        Backslash => 0x1c,
        CloseBracket => 0x1d,
        _ => return None,
    })
}

fn special_key(key: Key, m: Modifiers) -> Option<Vec<u8>> {
    let modp = modifier_param(m);
    Some(match key {
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Tab => {
            if m.shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] }
        }
        Key::Escape => vec![0x1b],

        Key::ArrowUp => csi_final(b'A', modp),
        Key::ArrowDown => csi_final(b'B', modp),
        Key::ArrowRight => csi_final(b'C', modp),
        Key::ArrowLeft => csi_final(b'D', modp),
        Key::Home => csi_final(b'H', modp),
        Key::End => csi_final(b'F', modp),

        Key::Insert => csi_tilde(2, modp),
        Key::Delete => csi_tilde(3, modp),
        Key::PageUp => csi_tilde(5, modp),
        Key::PageDown => csi_tilde(6, modp),

        Key::F1 => function_key(b'P', modp),
        Key::F2 => function_key(b'Q', modp),
        Key::F3 => function_key(b'R', modp),
        Key::F4 => function_key(b'S', modp),
        Key::F5 => csi_tilde(15, modp),
        Key::F6 => csi_tilde(17, modp),
        Key::F7 => csi_tilde(18, modp),
        Key::F8 => csi_tilde(19, modp),
        Key::F9 => csi_tilde(20, modp),
        Key::F10 => csi_tilde(21, modp),
        Key::F11 => csi_tilde(23, modp),
        Key::F12 => csi_tilde(24, modp),

        _ => return None,
    })
}

/// xterm modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
fn modifier_param(m: Modifiers) -> u8 {
    1 + (m.shift as u8) + ((m.alt as u8) << 1) + ((m.ctrl as u8) << 2)
}

fn csi_final(final_byte: u8, modp: u8) -> Vec<u8> {
    if modp == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{}{}", modp, final_byte as char).into_bytes()
    }
}

fn csi_tilde(code: u8, modp: u8) -> Vec<u8> {
    if modp == 1 {
        format!("\x1b[{}~", code).into_bytes()
    } else {
        format!("\x1b[{};{}~", code, modp).into_bytes()
    }
}

fn function_key(letter: u8, modp: u8) -> Vec<u8> {
    if modp == 1 {
        vec![0x1b, b'O', letter]
    } else {
        format!("\x1b[1;{}{}", modp, letter as char).into_bytes()
    }
}
