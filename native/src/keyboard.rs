//! Existing DOM HID names, USB usages and US/DE text mapping v1.
use crate::{
    GatewayError, Result,
    contracts::{Action, Layout, Validate},
};

pub fn key_name(key: &str) -> Result<String> {
    let alias = match key.to_ascii_uppercase().as_str() {
        "CTRL" | "CONTROL" => "ControlLeft",
        "SHIFT" => "ShiftLeft",
        "ALT" => "AltLeft",
        "ALTGR" => "AltRight",
        "META" | "SUPER" | "WIN" => "MetaLeft",
        "ENTER" | "RETURN" => "Enter",
        "ESC" | "ESCAPE" => "Escape",
        "TAB" => "Tab",
        "SPACE" => "Space",
        "BACKSPACE" => "Backspace",
        "DELETE" => "Delete",
        "UP" => "ArrowUp",
        "DOWN" => "ArrowDown",
        "LEFT" => "ArrowLeft",
        "RIGHT" => "ArrowRight",
        "HOME" => "Home",
        "END" => "End",
        "PAGEUP" => "PageUp",
        "PAGEDOWN" => "PageDown",
        "INSERT" => "Insert",
        "CAPSLOCK" => "CapsLock",
        "PRINTSCREEN" => "PrintScreen",
        _ => "",
    };
    if !alias.is_empty() {
        return Ok(alias.into());
    }
    if key.len() == 1 {
        let c = key.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return Ok(format!("Key{}", char::from(c.to_ascii_uppercase())));
        }
        if c.is_ascii_digit() {
            return Ok(format!("Digit{}", char::from(c)));
        }
    }
    let valid = match key {
        "ControlLeft" | "ControlRight" | "ShiftLeft" | "ShiftRight" | "AltLeft" | "AltRight"
        | "MetaLeft" | "MetaRight" | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight"
        | "Enter" | "Escape" | "Tab" | "Space" | "Backspace" | "Delete" | "Home" | "End"
        | "PageUp" | "PageDown" | "Insert" | "CapsLock" | "PrintScreen" | "Minus" | "Equal"
        | "BracketLeft" | "BracketRight" | "Backslash" | "Semicolon" | "Quote" | "Backquote"
        | "Comma" | "Period" | "Slash" | "IntlBackslash" => true,
        _ if key.starts_with("Key") => key.len() == 4 && key.as_bytes()[3].is_ascii_uppercase(),
        _ if key.starts_with("Digit") => key.len() == 6 && key.as_bytes()[5].is_ascii_digit(),
        _ => key
            .strip_prefix('F')
            .and_then(|s| s.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n) && key == format!("F{n}")),
    };
    if valid {
        Ok(key.into())
    } else {
        Err(GatewayError::new("UNSUPPORTED_ACTION"))
    }
}

pub fn text_keys(text: &str, layout: Layout) -> Result<Vec<Vec<String>>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        let chord: Vec<&str> = match c {
            '\r' => continue,
            '\n' => vec!["Enter"],
            '\t' => vec!["Tab"],
            ' ' => vec!["Space"],
            '´' | '`' | '^' if layout == Layout::De => {
                out.push(match c {
                    '´' => vec!["Equal".into()],
                    '`' => vec!["ShiftLeft".into(), "Equal".into()],
                    _ => vec!["Backquote".into()],
                });
                out.push(vec!["Space".into()]);
                continue;
            }
            _ if c.is_ascii_alphabetic() => {
                let mut letter = c.to_ascii_uppercase();
                if layout == Layout::De {
                    letter = match letter {
                        'Y' => 'Z',
                        'Z' => 'Y',
                        _ => letter,
                    };
                }
                let key = format!("Key{letter}");
                out.push(if c.is_ascii_uppercase() {
                    vec!["ShiftLeft".into(), key]
                } else {
                    vec![key]
                });
                continue;
            }
            _ if c.is_ascii_digit() => {
                out.push(vec![format!("Digit{c}")]);
                continue;
            }
            _ if layout == Layout::Us => {
                let codes = [
                    "Backquote",
                    "Minus",
                    "Equal",
                    "BracketLeft",
                    "BracketRight",
                    "Backslash",
                    "Semicolon",
                    "Quote",
                    "Comma",
                    "Period",
                    "Slash",
                ];
                if let Some(n) = "`-=[]\\;',./".chars().position(|v| v == c) {
                    vec![codes[n]]
                } else if let Some(n) = "~_+{}|:\"<>?".chars().position(|v| v == c) {
                    vec!["ShiftLeft", codes[n]]
                } else if let Some(n) = ")!@#$%^&*(".chars().position(|v| v == c) {
                    out.push(vec!["ShiftLeft".into(), format!("Digit{n}")]);
                    continue;
                } else {
                    return unsupported_text(c, layout);
                }
            }
            _ => match c {
                'ß' => vec!["Minus"],
                '?' => vec!["ShiftLeft", "Minus"],
                '+' => vec!["BracketRight"],
                '*' => vec!["ShiftLeft", "BracketRight"],
                'ü' => vec!["BracketLeft"],
                'Ü' => vec!["ShiftLeft", "BracketLeft"],
                'ö' => vec!["Semicolon"],
                'Ö' => vec!["ShiftLeft", "Semicolon"],
                'ä' => vec!["Quote"],
                'Ä' => vec!["ShiftLeft", "Quote"],
                '#' => vec!["Backslash"],
                '\'' => vec!["ShiftLeft", "Backslash"],
                '<' => vec!["IntlBackslash"],
                '>' => vec!["ShiftLeft", "IntlBackslash"],
                '|' => vec!["AltRight", "IntlBackslash"],
                ',' => vec!["Comma"],
                ';' => vec!["ShiftLeft", "Comma"],
                '.' => vec!["Period"],
                ':' => vec!["ShiftLeft", "Period"],
                '-' => vec!["Slash"],
                '_' => vec!["ShiftLeft", "Slash"],
                '@' => vec!["AltRight", "KeyQ"],
                '€' => vec!["AltRight", "KeyE"],
                '{' => vec!["AltRight", "Digit7"],
                '[' => vec!["AltRight", "Digit8"],
                ']' => vec!["AltRight", "Digit9"],
                '}' => vec!["AltRight", "Digit0"],
                '\\' => vec!["AltRight", "Minus"],
                '~' => vec!["AltRight", "BracketRight"],
                _ => {
                    if let Some(n) = "=!\"§$%&/()".chars().position(|v| v == c) {
                        out.push(vec!["ShiftLeft".into(), format!("Digit{n}")]);
                        continue;
                    }
                    return unsupported_text(c, layout);
                }
            },
        };
        out.push(chord.into_iter().map(str::to_owned).collect());
    }
    Ok(out)
}

fn unsupported_text<T>(c: char, layout: Layout) -> Result<T> {
    let layout = if layout == Layout::Us { "us" } else { "de" };
    Err(GatewayError::message(
        "UNSUPPORTED_TEXT",
        &format!("Character U+{:X} is unavailable for {layout}", u32::from(c)),
    ))
}

pub fn usb_key(input: &str) -> Result<u8> {
    let key = key_name(input)?;
    let modifiers = [
        "ControlLeft",
        "ShiftLeft",
        "AltLeft",
        "MetaLeft",
        "ControlRight",
        "ShiftRight",
        "AltRight",
        "MetaRight",
    ];
    if let Some(n) = modifiers.iter().position(|&m| m == key) {
        return Ok(224 + n as u8);
    }
    if let Some(letter) = key.strip_prefix("Key") {
        return Ok(letter.as_bytes()[0] - b'A' + 4);
    }
    if let Some(digit) = key.strip_prefix("Digit") {
        return Ok(if digit == "0" {
            39
        } else {
            29 + digit.as_bytes()[0] - b'0'
        });
    }
    if let Some(number) = key.strip_prefix('F').and_then(|s| s.parse::<u8>().ok()) {
        return Ok(if number <= 12 {
            57 + number
        } else {
            91 + number
        });
    }
    Ok(match key.as_str() {
        "Enter" => 40,
        "Escape" => 41,
        "Backspace" => 42,
        "Tab" => 43,
        "Space" => 44,
        "Minus" => 45,
        "Equal" => 46,
        "BracketLeft" => 47,
        "BracketRight" => 48,
        "Backslash" => 49,
        "Semicolon" => 51,
        "Quote" => 52,
        "Backquote" => 53,
        "Comma" => 54,
        "Period" => 55,
        "Slash" => 56,
        "CapsLock" => 57,
        "PrintScreen" => 70,
        "Insert" => 73,
        "Home" => 74,
        "PageUp" => 75,
        "Delete" => 76,
        "End" => 77,
        "PageDown" => 78,
        "ArrowRight" => 79,
        "ArrowLeft" => 80,
        "ArrowDown" => 81,
        "ArrowUp" => 82,
        "IntlBackslash" => 100,
        _ => return Err(GatewayError::new("UNSUPPORTED_ACTION")),
    })
}

/// Validate the *entire* batch before a driver sends its first HID event.
pub fn validate_hid(
    actions: &[Action],
    width: u32,
    height: u32,
    absolute_mouse: bool,
    keyboard_hid: bool,
    layout: Layout,
) -> Result<()> {
    for action in actions {
        action.validate()?;
        let check_point = |x: i64, y: i64| {
            if x >= i64::from(width) || y >= i64::from(height) {
                Err(GatewayError::new("VIEW_CHANGED"))
            } else {
                Ok(())
            }
        };
        if let Some(p) = action.point() {
            check_point(p.x, p.y)?;
        }
        if let Action::Drag { path } = action {
            for p in path {
                check_point(p.x, p.y)?;
            }
        }
        if matches!(
            action,
            Action::Click { .. }
                | Action::DoubleClick { .. }
                | Action::Move { .. }
                | Action::Drag { .. }
                | Action::Scroll { .. }
        ) && !absolute_mouse
        {
            return Err(GatewayError::message(
                "UNSUPPORTED_ACTION",
                "Absolute mouse mode is unavailable",
            ));
        }
        match action {
            Action::Keypress { keys } => {
                if !keyboard_hid {
                    return Err(GatewayError::new("UNSUPPORTED_ACTION"));
                }
                for key in keys {
                    key_name(key)?;
                }
            }
            Action::Type { text } => {
                if !keyboard_hid {
                    return Err(GatewayError::new("UNSUPPORTED_ACTION"));
                }
                text_keys(text, layout)?;
            }
            _ => (),
        }
    }
    Ok(())
}
