//! Reply-tag cycling and lookup.

#![allow(dead_code)]

use crate::cursors::ConversationKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    pub conversation_key: ConversationKey,
    pub message_id: String,
    pub plaintext_body: String,
}

pub struct TagTable {
    slots: [Option<MessageRef>; 26],
    next: usize,
    latest: Option<char>,
}

impl TagTable {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            next: 0,
            latest: None,
        }
    }

    pub fn assign(&mut self, msg: MessageRef) -> char {
        let idx = self.next % 26;
        let letter = (b'a' + idx as u8) as char;
        self.slots[idx] = Some(msg);
        self.next = self.next.wrapping_add(1);
        self.latest = Some(letter);
        letter
    }

    pub fn lookup(&self, letter: char) -> Option<&MessageRef> {
        if !letter.is_ascii_lowercase() {
            return None;
        }
        let idx = (letter as u8 - b'a') as usize;
        self.slots[idx].as_ref()
    }

    pub fn lookup_latest(&self) -> Option<&MessageRef> {
        self.latest.and_then(|c| self.lookup(c))
    }
}

impl Default for TagTable {
    fn default() -> Self {
        Self::new()
    }
}

// Strict rule from SPEC §Reply tags: single lowercase ASCII letter + space = tag.
// The spec has a conflicting parenthetical example ("alice is here" → tag 'a');
// we follow the stated rule and treat such inputs as body-only.
pub fn parse_reply(input: &str) -> (Option<char>, &str) {
    let mut chars = input.char_indices();
    let Some((_, c1)) = chars.next() else {
        return (None, input);
    };
    if c1.is_ascii_lowercase()
        && let Some((_, c2)) = chars.next()
        && c2 == ' '
    {
        return (Some(c1), &input[2..]);
    }
    (None, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ref(id: &str) -> MessageRef {
        MessageRef {
            conversation_key: ConversationKey::Chat {
                id: "conv-1".into(),
            },
            message_id: id.into(),
            plaintext_body: format!("body of {id}"),
        }
    }

    #[test]
    fn assigns_cyclically_a_through_z_then_wraps() {
        let mut t = TagTable::new();
        for i in 0..26 {
            let letter = t.assign(mk_ref(&format!("m{i}")));
            assert_eq!(letter, (b'a' + i) as char);
        }
        let letter = t.assign(mk_ref("m26"));
        assert_eq!(letter, 'a');
        assert_eq!(t.lookup('a').unwrap().message_id, "m26");
        assert_eq!(t.lookup('z').unwrap().message_id, "m25");
        assert_eq!(t.lookup('b').unwrap().message_id, "m1");
    }

    #[test]
    fn lookup_latest_returns_most_recent_assignment() {
        let mut t = TagTable::new();
        assert!(t.lookup_latest().is_none());
        t.assign(mk_ref("m0"));
        t.assign(mk_ref("m1"));
        assert_eq!(t.lookup_latest().unwrap().message_id, "m1");
    }

    #[test]
    fn lookup_rejects_non_lowercase_ascii() {
        let mut t = TagTable::new();
        t.assign(mk_ref("m0"));
        assert!(t.lookup('A').is_none());
        assert!(t.lookup('1').is_none());
        assert!(t.lookup('é').is_none());
    }

    #[test]
    fn parse_reply_tag_form() {
        assert_eq!(parse_reply("a hi there"), (Some('a'), "hi there"));
        assert_eq!(parse_reply("z "), (Some('z'), ""));
    }

    #[test]
    fn parse_reply_body_only() {
        assert_eq!(parse_reply("hi there"), (None, "hi there"));
    }

    #[test]
    fn parse_reply_alice_no_space_is_body() {
        assert_eq!(parse_reply("alice is here"), (None, "alice is here"));
    }

    #[test]
    fn parse_reply_empty() {
        assert_eq!(parse_reply(""), (None, ""));
    }

    #[test]
    fn parse_reply_uppercase_is_body() {
        assert_eq!(parse_reply("A hi"), (None, "A hi"));
    }

    #[test]
    fn parse_reply_single_letter_no_space_is_body() {
        assert_eq!(parse_reply("a"), (None, "a"));
    }
}
