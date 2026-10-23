//! Keys: key combos like `cmd+shift+g` and text, as X11 keysyms (RFC 6143 §7.5.4).

use anyhow::{bail, ensure};

use crate::base::Res;

/// The keysyms peephole names.
pub mod keysym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const PRINT: u32 = 0xff61;
    pub const INSERT: u32 = 0xff63;
    pub const MENU: u32 = 0xff67;
    pub const F1: u32 = 0xffbe;
    pub const SHIFT_L: u32 = 0xffe1;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const CAPS_LOCK: u32 = 0xffe5;
    pub const META_L: u32 = 0xffe7;
    pub const ALT_L: u32 = 0xffe9;
    pub const SUPER_L: u32 = 0xffeb;
    pub const DELETE: u32 = 0xffff;
    /// Unicode characters outside Latin-1 are `0x0100_0000 + code point`.
    pub const UNICODE_OFFSET: u32 = 0x0100_0000;
}

/// A key press or release.
pub type Stroke = (u32, bool);

/// The highest function key with an X11 keysym (F35).
const MAX_FUNCTION_KEY: u32 = 35;

/// Modifier names. `cmd` is `Super_L`, which macOS Screen Sharing reads as Command; `meta` stays
/// distinct (`Meta_L`) for servers that disagree.
fn modifier(name: &str) -> Option<u32> {
    Some(match name {
        "shift" => keysym::SHIFT_L,
        "ctrl" | "control" => keysym::CONTROL_L,
        "alt" | "option" | "opt" => keysym::ALT_L,
        "cmd" | "command" | "super" | "win" | "windows" => keysym::SUPER_L,
        "meta" => keysym::META_L,
        _ => return None,
    })
}

fn named_key(name: &str) -> Option<u32> {
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=MAX_FUNCTION_KEY).contains(&n)
    {
        return Some(keysym::F1 + n - 1);
    }
    Some(match name {
        "enter" | "return" => keysym::RETURN,
        "tab" => keysym::TAB,
        "esc" | "escape" => keysym::ESCAPE,
        "space" => u32::from(' '),
        "backspace" => keysym::BACKSPACE,
        "delete" | "del" | "forwarddelete" => keysym::DELETE,
        "insert" | "ins" => keysym::INSERT,
        "home" => keysym::HOME,
        "end" => keysym::END,
        "pageup" | "pgup" => keysym::PAGE_UP,
        "pagedown" | "pgdn" => keysym::PAGE_DOWN,
        "up" => keysym::UP,
        "down" => keysym::DOWN,
        "left" => keysym::LEFT,
        "right" => keysym::RIGHT,
        "capslock" => keysym::CAPS_LOCK,
        "menu" => keysym::MENU,
        "print" | "printscreen" => keysym::PRINT,
        "plus" => u32::from('+'),
        "minus" => u32::from('-'),
        _ => return None,
    })
}

/// The keysym that types `c`: Latin-1 as itself, other Unicode offset into the Unicode range.
/// Control characters have none.
#[must_use]
pub fn char_keysym(c: char) -> Option<u32> {
    match c {
        ' '..='~' | '\u{a0}'..='\u{ff}' => Some(u32::from(c)),
        c if c.is_control() => None,
        c => Some(keysym::UNICODE_OFFSET + u32::from(c)),
    }
}

/// A parsed key combo: modifiers held, in order, around one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: Vec<u32>,
    pub key: u32,
}

impl Combo {
    /// Parses `cmd+shift+g`, `ctrl+alt+delete`, `enter`, `f5`, `cmd++` and the like. Names are
    /// case-insensitive; a letter means its key, so `cmd+G` is `cmd+g` (add `shift` for capitals).
    ///
    /// # Errors
    ///
    /// On an empty combo, an unknown modifier or key, or a missing key after `+`.
    pub fn parse(spec: &str) -> Res<Self> {
        let spec = spec.trim();
        ensure!(!spec.is_empty(), "the key combo is empty");
        // `+` itself is a key: `+`, or `cmd++`.
        let (mods, key) = if spec == "+" {
            ("", "+")
        } else if let Some(mods) = spec.strip_suffix("++") {
            (mods, "+")
        } else {
            spec.rsplit_once('+').unwrap_or(("", spec))
        };

        let mut modifiers = Vec::new();
        for name in mods.split('+').map(str::trim).filter(|name| !name.is_empty()) {
            let Some(keysym) = modifier(&name.to_lowercase()) else {
                bail!("unknown modifier {name:?} in {spec:?}; use shift, ctrl, alt/option, cmd/super or meta");
            };
            if !modifiers.contains(&keysym) {
                modifiers.push(keysym);
            }
        }
        Ok(Self {
            modifiers,
            key: parse_key(key.trim(), spec)?,
        })
    }

    /// Presses the modifiers in order, taps the key, then releases the modifiers in reverse.
    #[must_use]
    pub fn strokes(&self) -> Vec<Stroke> {
        let mut out = Vec::with_capacity(self.modifiers.len() * 2 + 2);
        out.extend(self.modifiers.iter().map(|&m| (m, true)));
        out.extend([(self.key, true), (self.key, false)]);
        out.extend(self.modifiers.iter().rev().map(|&m| (m, false)));
        out
    }
}

fn parse_key(key: &str, spec: &str) -> Res<u32> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // A single character is its own key; letters are their unshifted key.
        return char_keysym(c.to_ascii_lowercase()).ok_or_else(|| anyhow::anyhow!("cannot press control character U+{:04X}", u32::from(c)));
    }
    ensure!(!key.is_empty(), "{spec:?} has no key after the last '+'");
    let name = key.to_lowercase().replace(['_', '-', ' '], "");
    named_key(&name)
        .or_else(|| modifier(&name))
        .ok_or_else(|| anyhow::anyhow!("unknown key {key:?} in {spec:?}; use a single character or a name like enter, tab, esc, space, backspace, delete, up, pageup, home or f1-f35"))
}

/// The strokes that type `text`: each character pressed and released, `\n` (or `\r\n`) as Return
/// and `\t` as Tab.
///
/// # Errors
///
/// On any other control character.
pub fn text_strokes(text: &str) -> Res<Vec<Stroke>> {
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let keysym = match c {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\n' | '\r' => keysym::RETURN,
            '\t' => keysym::TAB,
            c => char_keysym(c).ok_or_else(|| anyhow::anyhow!("cannot type control character U+{:04X}", u32::from(c)))?,
        };
        out.extend([(keysym, true), (keysym, false)]);
    }
    Ok(out)
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use pretty_assertions::assert_eq;

    use super::keysym::*;
    use super::*;

    fn combo(spec: &str) -> (Vec<u32>, u32) {
        let c = Combo::parse(spec).unwrap();
        (c.modifiers, c.key)
    }

    #[test]
    fn parses_modifier_combos() {
        assert_eq!(combo("cmd+shift+g"), (vec![SUPER_L, SHIFT_L], u32::from('g')));
        assert_eq!(combo("ctrl+alt+delete"), (vec![CONTROL_L, ALT_L], DELETE));
        assert_eq!(combo("Command+Option+Esc"), (vec![SUPER_L, ALT_L], ESCAPE));
        assert_eq!(combo("meta+c"), (vec![META_L], u32::from('c')));
        assert_eq!(combo(" ctrl + c "), (vec![CONTROL_L], u32::from('c')));
    }

    #[test]
    fn parses_named_and_single_keys() {
        assert_eq!(combo("enter"), (vec![], RETURN));
        assert_eq!(combo("Page_Up"), (vec![], PAGE_UP));
        assert_eq!(combo("f1"), (vec![], F1));
        assert_eq!(combo("F12"), (vec![], 0xffc9));
        assert_eq!(combo("space"), (vec![], 0x20));
        assert_eq!(combo("shift"), (vec![], SHIFT_L));
        assert_eq!(combo("/"), (vec![], u32::from('/')));
    }

    #[test]
    fn letters_mean_their_key_whatever_the_case() {
        assert_eq!(combo("cmd+G"), combo("cmd+g"));
    }

    #[test]
    fn plus_is_a_key_too() {
        assert_eq!(combo("+"), (vec![], u32::from('+')));
        assert_eq!(combo("cmd++"), (vec![SUPER_L], u32::from('+')));
        assert_eq!(combo("cmd+plus"), (vec![SUPER_L], u32::from('+')));
        assert_eq!(combo("-"), (vec![], u32::from('-')));
    }

    #[test]
    fn deduplicates_modifiers() {
        assert_eq!(combo("cmd+super+a"), (vec![SUPER_L], u32::from('a')));
    }

    #[test]
    fn rejects_nonsense() {
        for spec in ["", "   ", "cmd+", "hyper+a", "cmd+nosuchkey", "f36", "ctrl+\u{7}"] {
            assert!(Combo::parse(spec).is_err(), "{spec:?}");
        }
    }

    #[test]
    fn strokes_wrap_the_key_in_its_modifiers() {
        assert_eq!(
            Combo::parse("cmd+shift+g").unwrap().strokes(),
            [(SUPER_L, true), (SHIFT_L, true), (u32::from('g'), true), (u32::from('g'), false), (SHIFT_L, false), (SUPER_L, false),]
        );
    }

    #[test]
    fn maps_characters_to_keysyms() {
        assert_eq!(char_keysym('A'), Some(0x41));
        assert_eq!(char_keysym('é'), Some(0xe9));
        assert_eq!(char_keysym('€'), Some(0x0100_20ac));
        assert_eq!(char_keysym('😀'), Some(0x0101_f600));
        assert_eq!(char_keysym('\u{7}'), None);
    }

    #[test]
    fn types_text_with_line_breaks_and_tabs() {
        let keys: Vec<u32> = text_strokes("a\tB\r\nc\n").unwrap().iter().filter(|(_, down)| *down).map(|(k, _)| *k).collect();
        assert_eq!(keys, [u32::from('a'), TAB, u32::from('B'), RETURN, u32::from('c'), RETURN]);
        assert!(text_strokes("bell\u{7}").is_err());
    }
}
