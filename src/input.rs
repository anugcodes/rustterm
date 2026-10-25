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
///
/// `app_cursor` reflects DECCKM (`?1h`): when the remote program enabled
/// application cursor keys it expects SS3 (`ESC O A`) cursor keys rather than
/// the usual CSI (`ESC [ A`) form.
pub fn event_to_bytes(event: &egui::Event, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
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
        egui::Event::Copy => {
            // Ctrl+Shift+C is the copy-to-clipboard gesture, handled by the
            // caller. Bare Ctrl+C stays the terminal interrupt (ETX).
            if mods.ctrl && mods.shift && !mods.alt {
                None
            } else {
                Some(vec![0x03])
            }
        }
        egui::Event::Cut => Some(vec![0x18]), // Ctrl+X -> CAN
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
            encode_key(*key, *modifiers, app_cursor)
        }
        _ => None,
    }
}

/// True for the `Ctrl+Shift+C` "copy selection to clipboard" gesture.
///
/// egui-winit collapses *both* `Ctrl+C` and `Ctrl+Shift+C` into
/// `Event::Copy` (its `is_copy_command` ignores Shift) and never delivers the
/// raw key, so the frame's current modifiers are the only way to tell them
/// apart. `app.rs` uses this to copy the selection to the system clipboard
/// before `event_to_bytes` would turn the event into `ETX`.
pub fn is_copy_gesture(event: &egui::Event, mods: Modifiers) -> bool {
    matches!(event, egui::Event::Copy) && mods.ctrl && mods.shift && !mods.alt
}

/// Encode a key press that is not a plain printable character.
///
/// Returns `None` for keys whose text is delivered through `Event::Text`
/// (letters, digits, punctuation without Ctrl/Alt).
///
/// `app_cursor` reflects DECCKM (`?1h`): when the remote program enabled
/// application cursor keys it expects SS3 (`ESC O A`) cursor keys rather than
/// the usual CSI (`ESC [ A`) form.
pub fn encode_key(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    // Ctrl+<key> -> C0 control byte. Alt is excluded because AltGr on
    // Linux is often reported as Ctrl+Alt and should produce text.
    if m.ctrl && !m.alt {
        if let Some(b) = ctrl_byte(key) {
            return Some(vec![b]);
        }
    }

    special_key(key, m, app_cursor)
}

/// Universal ASCII control-code mapping for Ctrl+letter.
///
/// Ctrl+A = 0x01 (SOH) … Ctrl+Z = 0x1A (SUB). This is the same on every
/// Unix terminal; the remote program, not the client, decides what each
/// byte means.
fn ctrl_byte(key: Key) -> Option<u8> {
    use Key::*;
    Some(match key {
        A => 0x01,
        B => 0x02,
        C => 0x03,
        D => 0x04,
        E => 0x05,
        F => 0x06,
        G => 0x07,
        H => 0x08,
        I => 0x09,
        J => 0x0a,
        K => 0x0b,
        L => 0x0c,
        M => 0x0d,
        N => 0x0e,
        O => 0x0f,
        P => 0x10,
        Q => 0x11,
        R => 0x12,
        S => 0x13,
        T => 0x14,
        U => 0x15,
        V => 0x16,
        W => 0x17,
        X => 0x18,
        Y => 0x19,
        Z => 0x1a,
        Space => 0x00,
        OpenBracket => 0x1b, // Ctrl+[ == ESC
        Backslash => 0x1c,
        CloseBracket => 0x1d,
        _ => return None,
    })
}

fn special_key(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let modp = modifier_param(m);
    Some(match key {
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Tab => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],

        Key::ArrowUp => cursor_key(b'A', modp, app_cursor),
        Key::ArrowDown => cursor_key(b'B', modp, app_cursor),
        Key::ArrowRight => cursor_key(b'C', modp, app_cursor),
        Key::ArrowLeft => cursor_key(b'D', modp, app_cursor),
        Key::Home => cursor_key(b'H', modp, app_cursor),
        Key::End => cursor_key(b'F', modp, app_cursor),

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

/// Cursor / Home / End keys.
///
/// In application cursor key mode (DECCKM, `?1h`) the program expects the
/// SS3 form `ESC O A`; otherwise the CSI form `ESC [ A`.
///
/// This matters a lot: ncurses programs (htop, vim, mc, …) enable DECCKM
/// through terminfo's `smkx` capability and their key parser only recognises
/// the SS3 sequences. Sending CSI makes ncurses see a stray Escape followed by
/// literal `[` and `A` characters instead of KEY_UP.
///
/// Modified keys always use the CSI parameter form (`ESC [ 1 ; 5 A`), in both
/// modes — that is what xterm does.
fn cursor_key(final_byte: u8, modp: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor {
        if modp == 1 {
            vec![0x1b, b'O', final_byte]
        } else {
            format!("\x1b[1;{}{}", modp, final_byte as char).into_bytes()
        }
    } else if modp == 1 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, app_cursor: bool) -> Option<Vec<u8>> {
        encode_key(key, Modifiers::NONE, app_cursor)
    }

    /// Normal cursor key mode: CSI form.
    #[test]
    fn cursor_keys_are_csi_in_normal_mode() {
        assert_eq!(enc(Key::ArrowUp, false), Some(vec![0x1b, b'[', b'A']));
        assert_eq!(enc(Key::ArrowDown, false), Some(vec![0x1b, b'[', b'B']));
        assert_eq!(enc(Key::ArrowRight, false), Some(vec![0x1b, b'[', b'C']));
        assert_eq!(enc(Key::ArrowLeft, false), Some(vec![0x1b, b'[', b'D']));
        assert_eq!(enc(Key::Home, false), Some(vec![0x1b, b'[', b'H']));
        assert_eq!(enc(Key::End, false), Some(vec![0x1b, b'[', b'F']));
    }

    /// Application cursor key mode (DECCKM): SS3 form. ncurses programs such as
    /// htop only recognise this one.
    #[test]
    fn cursor_keys_are_ss3_in_application_mode() {
        assert_eq!(enc(Key::ArrowUp, true), Some(vec![0x1b, b'O', b'A']));
        assert_eq!(enc(Key::ArrowDown, true), Some(vec![0x1b, b'O', b'B']));
        assert_eq!(enc(Key::ArrowRight, true), Some(vec![0x1b, b'O', b'C']));
        assert_eq!(enc(Key::ArrowLeft, true), Some(vec![0x1b, b'O', b'D']));
        assert_eq!(enc(Key::Home, true), Some(vec![0x1b, b'O', b'H']));
        assert_eq!(enc(Key::End, true), Some(vec![0x1b, b'O', b'F']));
    }

    #[test]
    fn modified_cursor_keys_use_csi_parameter_form_in_both_modes() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            encode_key(Key::ArrowUp, ctrl, false),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode_key(Key::ArrowUp, ctrl, true),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn function_keys_f1_to_f4_use_ss3() {
        assert_eq!(enc(Key::F1, false), Some(vec![0x1b, b'O', b'P']));
        assert_eq!(enc(Key::F2, false), Some(vec![0x1b, b'O', b'Q']));
    }

    #[test]
    fn ctrl_c_is_etx_but_ctrl_shift_c_is_not() {
        let event = egui::Event::Copy;
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(event_to_bytes(&event, ctrl, false), Some(vec![0x03]));
        assert_eq!(event_to_bytes(&event, ctrl_shift, false), None);
        assert!(is_copy_gesture(&event, ctrl_shift));
        assert!(!is_copy_gesture(&event, ctrl));
    }

    #[test]
    fn paste_event_bare_ctrl_is_syn_and_shift_pastes_text() {
        let event = egui::Event::Paste("hello".to_string());
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(event_to_bytes(&event, ctrl, false), Some(vec![0x16]));
        assert_eq!(
            event_to_bytes(&event, ctrl_shift, false),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn ctrl_x_is_can() {
        let event = egui::Event::Cut;
        assert_eq!(
            event_to_bytes(&event, Modifiers::NONE, false),
            Some(vec![0x18])
        );
    }
}
