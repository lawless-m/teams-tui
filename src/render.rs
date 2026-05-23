//! HTML → plain text, message formatting, hard-wrap.

#![allow(dead_code)]

use scraper::Html;
use scraper::node::Node;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conversation {
    Channel {
        name: String,
        team: Option<String>,
    },
    Dm,
    GroupWithTopic {
        topic: String,
    },
    GroupNoTopic {
        participants: Vec<String>,
    },
}

pub fn prefix_for(conv: &Conversation) -> String {
    match conv {
        Conversation::Channel { name, team: None } => format!("#{name}"),
        Conversation::Channel {
            name,
            team: Some(team),
        } => format!("#{team}/{name}"),
        Conversation::Dm => "DM".to_string(),
        Conversation::GroupWithTopic { topic } => format!("#{topic}"),
        Conversation::GroupNoTopic { participants } => {
            let shown: Vec<&str> = participants.iter().take(3).map(|s| s.as_str()).collect();
            let mut s = format!("group[{}", shown.join(","));
            if participants.len() > 3 {
                s.push('…');
            }
            s.push(']');
            s
        }
    }
}

pub fn hard_wrap(text: &str, first_indent: &str, cont_indent: &str, width: usize) -> String {
    let first_w = width.saturating_sub(first_indent.chars().count()).max(1);
    let cont_w = width.saturating_sub(cont_indent.chars().count()).max(1);

    let mut out = String::new();
    let mut is_first_visual = true;

    let logical_lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    };

    for logical in logical_lines {
        let chars: Vec<char> = logical.chars().collect();
        if chars.is_empty() {
            if !is_first_visual {
                out.push('\n');
            }
            let indent = if is_first_visual {
                first_indent
            } else {
                cont_indent
            };
            out.push_str(indent);
            is_first_visual = false;
            continue;
        }

        let mut i = 0;
        while i < chars.len() {
            if !is_first_visual {
                out.push('\n');
            }
            let (indent, w) = if is_first_visual {
                (first_indent, first_w)
            } else {
                (cont_indent, cont_w)
            };
            out.push_str(indent);
            let end = (i + w).min(chars.len());
            for &c in &chars[i..end] {
                out.push(c);
            }
            i = end;
            is_first_visual = false;
        }
    }

    out
}

pub fn format_message(
    time_hhmm: &str,
    prefix: &str,
    sender: &str,
    tag: char,
    content: &str,
    width: usize,
) -> String {
    let leader = format!("{time_hhmm} {prefix}:{sender}: [{tag}] ");
    let indent = " ".repeat(leader.chars().count());
    hard_wrap(content, &leader, &indent, width)
}

pub fn html_to_text(html: &str, self_id: &str) -> (String, bool) {
    let doc = Html::parse_fragment(html);
    let mut out = String::new();
    let mut mention = false;
    for child in doc.tree.root().children() {
        render_node(child, self_id, &mut out, &mut mention);
    }
    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    (out, mention)
}

fn render_node(
    node: ego_tree::NodeRef<'_, Node>,
    self_id: &str,
    out: &mut String,
    mention: &mut bool,
) {
    match node.value() {
        Node::Text(t) => out.push_str(&t.text),
        Node::Element(el) => match el.name() {
            "at" => {
                if el.attr("id").unwrap_or("") == self_id {
                    *mention = true;
                }
                out.push('@');
                for c in node.children() {
                    render_node(c, self_id, out, mention);
                }
            }
            "emoji" => {
                if let Some(alt) = el.attr("alt") {
                    out.push_str(alt);
                }
            }
            "a" => {
                let href = el.attr("href").unwrap_or("").to_string();
                for c in node.children() {
                    render_node(c, self_id, out, mention);
                }
                if !href.is_empty() {
                    out.push_str(" (");
                    out.push_str(&href);
                    out.push(')');
                }
            }
            "img" => {
                out.push_str("[image]");
            }
            "blockquote" => {
                let mut inner = String::new();
                for c in node.children() {
                    render_node(c, self_id, &mut inner, mention);
                }
                ensure_newline(out);
                for line in inner.trim_end_matches('\n').lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            "pre" => {
                ensure_blank_line(out);
                for c in node.children() {
                    render_node(c, self_id, out, mention);
                }
                ensure_blank_line(out);
            }
            "br" => {
                out.push('\n');
            }
            "p" => {
                for c in node.children() {
                    render_node(c, self_id, out, mention);
                }
                ensure_newline(out);
            }
            _ => {
                for c in node.children() {
                    render_node(c, self_id, out, mention);
                }
            }
        },
        _ => {}
    }
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if out.ends_with("\n\n") {
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let (text, mention) = html_to_text("Hello there", "self");
        assert_eq!(text, "Hello there");
        assert!(!mention);
    }

    #[test]
    fn paragraphs_produce_newlines() {
        let (text, _) = html_to_text("<p>One</p><p>Two</p>", "self");
        assert_eq!(text, "One\nTwo");
    }

    #[test]
    fn br_produces_newline() {
        let (text, _) = html_to_text("Line1<br>Line2", "self");
        assert_eq!(text, "Line1\nLine2");
    }

    #[test]
    fn at_mention_of_self_sets_flag() {
        let (text, mention) = html_to_text(r#"Hi <at id="self">me</at>!"#, "self");
        assert_eq!(text, "Hi @me!");
        assert!(mention);
    }

    #[test]
    fn at_mention_of_other_does_not_set_flag() {
        let (text, mention) = html_to_text(r#"Hi <at id="alice">Alice</at>"#, "self");
        assert_eq!(text, "Hi @Alice");
        assert!(!mention);
    }

    #[test]
    fn emoji_uses_alt_text() {
        let (text, _) = html_to_text(r#"ok <emoji alt=":)"></emoji>"#, "self");
        assert_eq!(text, "ok :)");
    }

    #[test]
    fn anchor_renders_text_and_url() {
        let (text, _) = html_to_text(r#"see <a href="https://example.com">this</a>"#, "self");
        assert_eq!(text, "see this (https://example.com)");
    }

    #[test]
    fn anchor_without_href_renders_text_only() {
        let (text, _) = html_to_text(r#"<a>click</a>"#, "self");
        assert_eq!(text, "click");
    }

    #[test]
    fn img_renders_as_placeholder() {
        let (text, _) = html_to_text(r#"before <img src="x"/> after"#, "self");
        assert_eq!(text, "before [image] after");
    }

    #[test]
    fn blockquote_prefixes_lines() {
        let (text, _) = html_to_text(
            "<blockquote><p>previous</p></blockquote>reply text",
            "self",
        );
        assert_eq!(text, "> previous\nreply text");
    }

    #[test]
    fn pre_has_surrounding_blank_lines_and_preserves_text() {
        let (text, _) = html_to_text(
            "before<pre><code>let x = 1;</code></pre>after",
            "self",
        );
        assert_eq!(text, "before\n\nlet x = 1;\n\nafter");
    }

    #[test]
    fn unknown_tag_recurses_into_children() {
        let (text, _) = html_to_text("<div><span>nested <b>text</b></span></div>", "self");
        assert_eq!(text, "nested text");
    }

    #[test]
    fn prefix_channel_no_team() {
        assert_eq!(
            prefix_for(&Conversation::Channel {
                name: "IT-Support".into(),
                team: None,
            }),
            "#IT-Support"
        );
    }

    #[test]
    fn prefix_channel_with_team_disambiguation() {
        assert_eq!(
            prefix_for(&Conversation::Channel {
                name: "general".into(),
                team: Some("Engineering".into()),
            }),
            "#Engineering/general"
        );
    }

    #[test]
    fn prefix_dm() {
        assert_eq!(prefix_for(&Conversation::Dm), "DM");
    }

    #[test]
    fn prefix_group_with_topic() {
        assert_eq!(
            prefix_for(&Conversation::GroupWithTopic {
                topic: "project-x".into()
            }),
            "#project-x"
        );
    }

    #[test]
    fn prefix_group_untitled_three_or_fewer() {
        assert_eq!(
            prefix_for(&Conversation::GroupNoTopic {
                participants: vec!["Alice".into(), "Bob".into(), "Carol".into()],
            }),
            "group[Alice,Bob,Carol]"
        );
    }

    #[test]
    fn prefix_group_untitled_truncates_over_three() {
        assert_eq!(
            prefix_for(&Conversation::GroupNoTopic {
                participants: vec![
                    "Alice".into(),
                    "Bob".into(),
                    "Carol".into(),
                    "Dave".into(),
                    "Eve".into(),
                ],
            }),
            "group[Alice,Bob,Carol…]"
        );
    }

    #[test]
    fn hard_wrap_splits_long_content() {
        let s = hard_wrap("abcdefghij", "-> ", "   ", 7);
        assert_eq!(s, "-> abcd\n   efgh\n   ij");
    }

    #[test]
    fn hard_wrap_preserves_input_newlines_with_continuation_indent() {
        let s = hard_wrap("one\ntwo", ">> ", "   ", 20);
        assert_eq!(s, ">> one\n   two");
    }

    #[test]
    fn format_message_wraps_content_under_leader() {
        let s = format_message(
            "10:30",
            "#chan",
            "alice",
            'a',
            "hello world!",
            30,
        );
        let leader_len = "10:30 #chan:alice: [a] ".chars().count();
        assert_eq!(leader_len, 23);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[0].starts_with("10:30 #chan:alice: [a] "));
        for l in &lines[1..] {
            assert!(
                l.starts_with(&" ".repeat(leader_len)),
                "continuation line not indented: {l:?}"
            );
        }
        let joined: String = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    &l[leader_len..]
                } else {
                    &l[leader_len..]
                }
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(joined, "hello world!");
    }

    #[test]
    fn realistic_mixed_sample() {
        let html = r#"<div><blockquote><p>question?</p></blockquote><p>Hi <at id="self">me</at>, see <a href="https://example.com">this</a>.</p></div>"#;
        let (text, mention) = html_to_text(html, "self");
        assert_eq!(
            text,
            "> question?\nHi @me, see this (https://example.com)."
        );
        assert!(mention);
    }
}
