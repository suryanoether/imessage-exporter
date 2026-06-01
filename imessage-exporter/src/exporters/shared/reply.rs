use imessage_database::tables::messages::Message;

use crate::{
    app::error::RuntimeError,
    exporters::{
        formatter::{MessageFormatter, PartBodyBuilder, RenderContext},
        shared::driver::apply_body,
    },
};

/// Maximum character length of a reply / in-reaction-to snippet before
/// it gets truncated. Set high enough to keep typical messages intact —
/// short text fits, only genuinely long messages get the ellipsis.
const REPLY_SNIPPET_MAX_CHARS: usize = 256;

/// Build a short, single-line preview of the message being replied to. The
/// caller is responsible for HTML-escaping the result.
pub(crate) fn build_reply_snippet(parent: &Message) -> String {
    let raw = parent.text.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return non_text_placeholder(parent).to_string();
    }
    // Collapse internal whitespace (multi-line, tabs) into single spaces so
    // the preview stays one visual line.
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= REPLY_SNIPPET_MAX_CHARS {
        return collapsed;
    }
    let mut truncated: String = collapsed
        .chars()
        .take(REPLY_SNIPPET_MAX_CHARS)
        .collect();
    truncated.push('…');
    truncated
}

fn non_text_placeholder(parent: &Message) -> &'static str {
    if parent.num_attachments > 0 {
        "[attachment]"
    } else {
        "[no preview]"
    }
}

/// One reply, as fed to the format's `replies` template. `body` is already a
/// fully-rendered, format-safe payload; its concrete type `S` is chosen by
/// the calling format. `guid` is exposed for templates that need it (e.g. as
/// a per-reply anchor id); implementations that don't need it may ignore the
/// field.
pub(crate) struct ReplyEntry<S> {
    pub guid: String,
    pub body: S,
}

/// Render the tapbacks attached to `message[idx]`. Returns `None` when the
/// message has no tapbacks for this part *or* every tapback rendered empty
/// (e.g. all were [`TapbackAction::Removed`](imessage_database::message_types::variants::TapbackAction::Removed)).
/// `wrap` lifts each per-tapback rendered string into the format's payload
/// type.
pub(crate) fn build_tapbacks<'a, F, T>(
    formatter: &'a F,
    message: &'a Message,
    idx: usize,
    wrap: impl Fn(String) -> T,
) -> Result<Option<Vec<T>>, RuntimeError>
where
    F: MessageFormatter<'a> + PartBodyBuilder,
{
    let Some(tapbacks) = formatter
        .config()
        .tapbacks
        .get(&message.guid)
        .and_then(|m| m.get(&idx))
    else {
        return Ok(None);
    };

    let mut rendered = Vec::new();
    for tapback in tapbacks {
        let f = formatter.format_tapback(tapback)?;
        if !f.is_empty() {
            rendered.push(wrap(f));
        }
    }
    if rendered.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rendered))
    }
}

/// Render the replies threaded under a message part. Tapbacks in the reply
/// list are skipped (they render alongside their parent via `build_tapbacks`).
/// `buffer_capacity` is the format's `MessageWriter::BUFFER_CAPACITY` (used
/// to pre-allocate the per-reply scratch buffer). `wrap_body` lifts the
/// rendered reply body into the format's payload type.
pub(crate) fn build_replies<'a, F, S>(
    formatter: &'a F,
    replies: Option<&'a mut Vec<Message>>,
    buffer_capacity: usize,
    wrap_body: impl Fn(String) -> S,
) -> Result<Option<Vec<ReplyEntry<S>>>, RuntimeError>
where
    F: MessageFormatter<'a> + PartBodyBuilder,
{
    let Some(replies) = replies else {
        return Ok(None);
    };

    let mut rendered = Vec::new();
    for reply in replies.iter_mut() {
        apply_body(reply, formatter.config().data_source.db());
        if !reply.is_tapback() {
            let mut buf = String::with_capacity(buffer_capacity);
            formatter.format_message_into(reply, RenderContext::Reply, &mut buf)?;
            rendered.push(ReplyEntry {
                guid: reply.guid.clone(),
                body: wrap_body(buf),
            });
        }
    }
    if rendered.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rendered))
    }
}

#[cfg(test)]
mod snippet_tests {
    use super::{REPLY_SNIPPET_MAX_CHARS, build_reply_snippet};
    use crate::Config;
    use imessage_database::tables::messages::Message;

    fn parent_with(text: Option<&str>, attachments: i32) -> Message {
        let mut m = Config::fake_message();
        m.text = text.map(str::to_string);
        m.num_attachments = attachments;
        m
    }

    #[test]
    fn snippet_returns_short_text_unchanged() {
        let parent = parent_with(Some("hi there"), 0);
        assert_eq!(build_reply_snippet(&parent), "hi there");
    }

    #[test]
    fn snippet_collapses_internal_whitespace() {
        let parent = parent_with(Some("line one\nline\ttwo   end"), 0);
        assert_eq!(build_reply_snippet(&parent), "line one line two end");
    }

    #[test]
    fn snippet_truncates_long_text_with_ellipsis() {
        let parent = parent_with(Some(&"a".repeat(REPLY_SNIPPET_MAX_CHARS + 25)), 0);
        let out = build_reply_snippet(&parent);
        assert_eq!(out.chars().count(), REPLY_SNIPPET_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn snippet_keeps_boundary_length_intact() {
        let parent = parent_with(Some(&"a".repeat(REPLY_SNIPPET_MAX_CHARS)), 0);
        let out = build_reply_snippet(&parent);
        assert_eq!(out.chars().count(), REPLY_SNIPPET_MAX_CHARS);
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn snippet_falls_back_to_attachment_placeholder() {
        let parent = parent_with(None, 1);
        assert_eq!(build_reply_snippet(&parent), "[attachment]");
    }

    #[test]
    fn snippet_falls_back_to_no_preview_when_empty_and_no_attachment() {
        let parent = parent_with(Some("   "), 0);
        assert_eq!(build_reply_snippet(&parent), "[no preview]");
    }
}
