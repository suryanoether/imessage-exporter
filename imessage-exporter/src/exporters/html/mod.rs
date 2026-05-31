use std::{
    borrow::Cow,
    cmp::{
        Ordering::{Equal, Greater, Less},
        min,
    },
    fs::File,
    io::{BufWriter, Write},
};

use crate::{
    app::{error::RuntimeError, runtime::Config, sanitizers::sanitize_html},
    exporters::{
        formatter::{
            AttachmentRender, MessageFormatter, PartBodyBuilder, RenderContext, TextEffectFormatter,
        },
        shared::{
            announcement::{AnnouncementBody, resolve_announcement},
            attachment::prepare_attachment,
            balloon::dispatch_app_balloon,
            driver::{ExportState, MessageWriter, apply_body},
            edited::{EditDiff, normalize_edited},
            message::MessageContext,
            part::dispatch_part_body,
            render::{render_template, render_template_into},
            reply::{build_replies, build_reply_snippet, build_tapbacks},
            tapback::resolve_tapback,
            time::{format_message_date, format_timestamp, message_time},
        },
    },
};

use imessage_database::{
    message_types::{
        edited::EditedMessage,
        text_effects::TextEffect,
        variants::{Announcement, Tapback, TapbackAction, Variant},
    },
    tables::{
        attachment::{Attachment, MediaType},
        messages::{
            Message,
            models::{AttachmentMeta, BubbleComponent, SharedLocation, TextAttributes},
        },
        table::YOU,
    },
};

mod balloons;
mod safe;
mod text_effects;
mod view_model;

use safe::Html;
use view_model::{
    AnnouncementInnerVM, AttachmentVM, AttachmentVariant, EditedRow, EditedVM, ForensicMetaVM,
    InReactionToVM, MessagePartVM, MessageVM, PartBody, RepliesVM, ReplyAnchorKind, ReplyingToVM,
    StickerSuffixVM, TapbackBubbleVM, TapbackVM, TapbacksVM,
};

// MARK: HTML
const HEADER: &str = "<html>\n<head>\n<meta charset=\"UTF-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">";
const FOOTER: &str = "</body></html>";
const STYLE: &str = include_str!("resources/style.css");

#[derive(Debug, Clone)]
/// [`EventType`] is used to track the start and end of HTML text attributes
/// so we can render them correctly in the HTML output.
enum EventType<'a> {
    /// Start event for text attributes, contains the index of the attribute
    Start(usize, &'a [TextEffect]),
    /// End event for text attributes, contains the index of the attribute
    End(usize),
}

pub struct HTML<'a> {
    /// Data that is setup from the application's runtime
    pub config: &'a Config,
    /// Shared per-export state (file cache, orphaned writer, progress bar).
    pub state: ExportState,
}

impl<'a> HTML<'a> {
    pub fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        Ok(HTML {
            config,
            state: ExportState::new(config, "html")?,
        })
    }
}

// MARK: Driver hooks
impl<'a> MessageWriter<'a> for HTML<'a> {
    const LABEL: &'static str = "html";
    const BUFFER_CAPACITY: usize = 2048;

    fn config(&self) -> &'a Config {
        self.config
    }

    fn state(&self) -> &ExportState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut ExportState {
        &mut self.state
    }

    fn write_file_header(file: &mut BufWriter<File>) -> Result<(), RuntimeError> {
        HTML::write_headers(file)
    }

    fn write_file_footer(file: &mut BufWriter<File>) -> Result<(), RuntimeError> {
        file.write_all(FOOTER.as_bytes())?;
        Ok(())
    }

    fn footer_notice() -> Option<&'static str> {
        Some("Writing HTML footers...")
    }
}

// MARK: Writer
impl<'a> MessageFormatter<'a> for HTML<'a> {
    fn format_attachment(
        &self,
        attachment: &'a mut Attachment,
        message: &Message,
        metadata: &AttachmentMeta,
    ) -> AttachmentRender {
        if let Err(render) = prepare_attachment(self.config, &self.state, attachment, message) {
            return render;
        }

        let embed_path = self.config.message_attachment_path(attachment);

        let variant = match attachment.mime_type() {
            MediaType::Image(_) => AttachmentVariant::Image,
            // Video duplicates the source tag intentionally; see
            // https://github.com/ReagentX/imessage-exporter/issues/73
            MediaType::Video(media_type) => AttachmentVariant::Video { media_type },
            MediaType::Audio(media_type) => match metadata.transcription.as_deref() {
                Some(transcription) => AttachmentVariant::AudioTranscription {
                    media_type,
                    transcription,
                },
                None => AttachmentVariant::Audio { media_type },
            },
            MediaType::Text(_) | MediaType::Application(_) => {
                let Some(filename) = attachment.filename() else {
                    return AttachmentRender::MissingFilename;
                };
                AttachmentVariant::Download {
                    filename,
                    file_size: attachment.file_size(),
                }
            }
            MediaType::Unknown => {
                if attachment
                    .copied_path
                    .as_ref()
                    .is_some_and(|path| path.is_dir())
                {
                    let Some(filename) = attachment.filename() else {
                        return AttachmentRender::MissingFilename;
                    };
                    AttachmentVariant::UnknownFolder {
                        filename,
                        file_size: attachment.file_size(),
                    }
                } else {
                    AttachmentVariant::UnknownOther {
                        file_size: attachment.file_size(),
                    }
                }
            }
            MediaType::Other(media_type) => AttachmentVariant::Other { media_type },
        };

        AttachmentRender::Embedded(render_template(&AttachmentVM {
            lazy: !self.config.options.no_lazy,
            embed_path,
            variant,
        }))
    }

    fn format_sticker(&self, sticker: &'a mut Attachment, message: &Message) -> String {
        let mut sticker_embed =
            match self.format_attachment(sticker, message, &AttachmentMeta::default()) {
                AttachmentRender::Embedded(html) => html,
                AttachmentRender::MissingFilename => return String::new(),
                AttachmentRender::NamedFile(name) => return sanitize_html(&name).into_owned(),
            };

        if let Some(kind) = sticker.get_sticker_decoration(
            self.config.data_source.db(),
            &self.config.options.platform,
            &self.config.options.db_path,
            self.config.options.attachment_root.as_deref(),
        ) {
            let suffix_html = render_template(&StickerSuffixVM { kind });
            sticker_embed.push_str(&suffix_html);
        }

        sticker_embed
    }

    fn format_app(
        &self,
        message: &'a Message,
        attachments: &mut Vec<Attachment>,
    ) -> Result<String, RuntimeError> {
        Ok(dispatch_app_balloon(
            self,
            message,
            attachments,
            self.config,
        )?)
    }

    fn format_tapback(&self, msg: &Message) -> Result<String, RuntimeError> {
        let Some(resolved) = resolve_tapback(msg, self.config, |sticker| {
            Html::trust(self.format_sticker(sticker, msg))
        })?
        else {
            return Ok(String::new());
        };
        let (action_label, extra_class, time_html) = match &resolved.forensic {
            None => ("", "", Html::trust(String::new())),
            Some(f) => {
                let (action_label, extra_class) = match f.action {
                    TapbackAction::Added => ("", ""),
                    TapbackAction::Removed => ("removed ", " tapback_removed"),
                };
                let time_html = Html::trust(format!(
                    "<div class=\"tapback_time\">{}</div>",
                    f.timestamp
                ));
                (action_label, extra_class, time_html)
            }
        };
        Ok(render_template(&TapbackVM {
            kind: resolved.kind,
            action_label,
            extra_class,
            time_html,
        }))
    }

    fn format_tapback_bubble(&self, msg: &Message) -> Result<String, RuntimeError> {
        let Variant::Tapback(_, action, tapback) = msg.variant() else {
            return Ok(String::new());
        };
        let is_removed = matches!(action, TapbackAction::Removed);
        let action_word = if is_removed { "Removed" } else { "Added" };
        // Tapback::Sticker, ::Loved, etc. render via Display; Emoji renders
        // the bare emoji. Sanitize the resulting string so user-supplied
        // emoji metadata can't smuggle markup.
        let kind_string = match tapback {
            Tapback::Sticker => "Sticker".to_string(),
            other => format!("{other}"),
        };
        let kind_html = Html::trust(sanitize_html(&kind_string).into_owned());
        let sender = self
            .config
            .who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id)
            .to_string();
        let timestamp = format_message_date(msg, self.config.offset);
        let in_reaction_to = self.resolve_in_reaction_to(msg);
        let guid_short = short_guid(&msg.guid);
        let service = format!("{}", msg.service());
        Ok(render_template(&TapbackBubbleVM {
            guid: msg.guid.clone(),
            guid_short,
            is_from_me: msg.is_from_me(),
            service,
            timestamp,
            sender,
            kind_html,
            action_word,
            is_removed,
            in_reaction_to,
        }))
    }

    fn format_announcement(&self, msg: &Message, out: &mut String) {
        let (kind, wrap_newlines) = match resolve_announcement(msg, self.config, YOU) {
            None => (AnnouncementBody::Unknown, true),
            Some(resolved) => {
                let wrap = !matches!(resolved.announcement, Announcement::FullyUnsent);
                (resolved.into(), wrap)
            }
        };

        if wrap_newlines {
            out.push('\n');
        }
        render_template_into(&AnnouncementInnerVM { kind }, out);
        if wrap_newlines {
            out.push('\n');
        }
    }

    fn format_shareplay(&self) -> &'static str {
        "<hr>SharePlay Message Ended"
    }

    fn format_shared_location(&self, kind: SharedLocation) -> &'static str {
        match kind {
            SharedLocation::Started => "<hr>Started sharing location!",
            SharedLocation::Stopped => "<hr>Stopped sharing location!",
        }
    }

    fn format_edited(
        &self,
        msg: &'a Message,
        edited_message: &'a EditedMessage,
        message_part_idx: usize,
    ) -> Option<String> {
        let kind = normalize_edited(msg, edited_message, message_part_idx, self.config, YOU)?
            .map_rows(|event| {
                let rendered_text =
                    if let Some(BubbleComponent::Text(attributes)) = event.components.first() {
                        self.format_attributes(event.text, attributes)
                    } else {
                        sanitize_html(event.text).into_owned()
                    };
                let timestamp = match event.diff_since_previous {
                    EditDiff::First => String::new(),
                    EditDiff::Failed => "Edited later".to_string(),
                    EditDiff::Computed(diff) => format!("Edited {diff} later"),
                };
                EditedRow {
                    is_last: event.is_last,
                    timestamp,
                    text_html: Html::trust(rendered_text),
                }
            });

        Some(render_template(&EditedVM { kind }))
    }

    fn format_attributes(&self, text: &str, attributes: &[TextAttributes]) -> String {
        if attributes.is_empty() {
            return sanitize_html(text).into_owned();
        }

        // Create events for attribute starts and ends
        let mut events = Vec::new();

        // Create events for each attribute, marking start and end positions. The ID is the index of the attribute in the list.
        for (attr_id, attr) in attributes.iter().enumerate() {
            events.push((attr.start, EventType::Start(attr_id, &attr.effects)));
            events.push((attr.end, EventType::End(attr_id)));
        }

        // Sort events by position, with ends before starts at the same position
        events.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| match (&a.1, &b.1) {
                (EventType::End(_), EventType::Start(_, _)) => Less,
                (EventType::Start(_, _), EventType::End(_)) => Greater,
                _ => Equal,
            })
        });

        let mut result = String::new();
        // The currently active attributes, stored as (attribute ID, TextAttributes)
        let mut active_attrs = Vec::new();
        let mut last_pos = events.first().map_or(0, |(pos, _)| *pos);

        for (pos, event) in events {
            // Add text before this event with current active attributes
            if pos > last_pos && last_pos < text.len() {
                // Get the text slice from last position to current position
                let end_pos = min(pos, text.len());
                let text_slice = &text[last_pos..end_pos];
                // Sanitize the text slice
                let sanitized_text = sanitize_html(text_slice);
                result.push_str(&self.apply_active_attributes(&sanitized_text, &active_attrs));
            }

            // Update active attributes based on the event
            match event {
                EventType::Start(attr_id, attr) => {
                    // Add the attribute that starts
                    active_attrs.push((attr_id, attr));
                }
                EventType::End(attr_id) => {
                    // Remove the attribute that ends
                    active_attrs.retain(|(id, _)| *id != attr_id);
                }
            }

            last_pos = pos;
        }
        result
    }

    fn format_message_into(
        &self,
        message: &Message,
        context: RenderContext,
        out: &mut String,
    ) -> Result<(), RuntimeError> {
        let is_reply = matches!(context, RenderContext::Reply);
        let forensic = self.config.options.forensic;
        let mut ctx = MessageContext::resolve(message, self.config.data_source.db())?;
        let mut attachment_index: usize = 0;

        let mut parts = Vec::with_capacity(message.components.len());
        for (idx, message_part) in message.components.iter().enumerate() {
            let body = dispatch_part_body(
                self,
                message,
                idx,
                message_part,
                &mut ctx.attachments,
                &mut attachment_index,
            );

            // In forensic mode every message renders at its own chronological
            // position; replies are not inlined under their parent, which is
            // what produces the duplication.
            let replies = if forensic {
                None
            } else {
                build_replies(
                    self,
                    ctx.replies_map.get_mut(&idx),
                    Self::BUFFER_CAPACITY,
                    Html::trust,
                )?
                .map(|replies| RepliesVM { replies })
            };

            // In forensic mode tapbacks render as their own timeline
            // bubbles (see `format_tapback_bubble`), so don't also render
            // them under the message they reacted to.
            let tapbacks = if forensic {
                None
            } else {
                build_tapbacks(self, message, idx, Html::trust)?
                    .map(|tapbacks| TapbacksVM { tapbacks })
            };

            parts.push(MessagePartVM {
                body,
                expressive: ctx.expressive,
                tapbacks,
                replies,
            });
        }

        let (date, read_after) = self.get_time(message);

        // Forensic mode replaces the original anchor/trailing-context pair
        // with `replying_to` + a stable per-message anchor (`id="{guid}"`),
        // so dropping the legacy anchor/context fields keeps the markup
        // from carrying two competing reply affordances.
        let reply_anchor = if !forensic && message.is_reply() {
            Some(if is_reply {
                ReplyAnchorKind::InThread
            } else {
                ReplyAnchorKind::TopLevel
            })
        } else {
            None
        };
        let trailing_reply_context = !forensic && message.is_reply() && !is_reply;
        let anchor_attr = if forensic && !is_reply {
            Some(message.guid.clone())
        } else if !forensic && message.is_reply() && !is_reply {
            Some(format!("r-{}", message.guid))
        } else {
            None
        };

        let replying_to = if forensic && message.is_reply() && !is_reply {
            self.resolve_replying_to(message)
        } else {
            None
        };

        let forensic_meta = if forensic {
            Some(self.build_forensic_meta(message))
        } else {
            None
        };

        let vm = MessageVM {
            guid: &message.guid,
            anchor_attr,
            replying_to,
            is_from_me: message.is_from_me(),
            service: message.service(),
            date,
            read_after,
            reply_anchor,
            sender: self.config.who(
                message.handle_id,
                message.is_from_me(),
                &message.destination_caller_id,
            ),
            is_deleted: message.is_deleted(),
            subject: message.subject.as_deref(),
            shareplay: message
                .is_shareplay()
                .then(|| Html::trust(self.format_shareplay())),
            shared_location: message
                .shared_location_kind()
                .map(|kind| Html::trust(self.format_shared_location(kind))),
            parts,
            trailing_reply_context,
            forensic_meta,
        };
        render_template_into(&vm, out);
        Ok(())
    }
}

// MARK: Part Body
impl PartBodyBuilder for HTML<'_> {
    type Body = PartBody;

    fn body_empty(&self) -> Self::Body {
        PartBody::Empty
    }

    fn body_text_bubble(&self, content: String) -> Self::Body {
        PartBody::TextBubble {
            html: Html::trust(content),
        }
    }

    fn body_text_translated(&self, translated: String, original: String) -> Self::Body {
        PartBody::TextTranslated {
            translated: Html::trust(translated),
            original: Html::trust(original),
        }
    }

    fn body_text_edited(&self, content: String) -> Self::Body {
        PartBody::TextEdited {
            html: Html::trust(content),
        }
    }

    fn body_attachment(&self, content: String) -> Self::Body {
        PartBody::Attachment {
            html: Html::trust(content),
        }
    }

    fn body_attachment_error(&self, error: &str) -> Self::Body {
        PartBody::AttachmentError {
            error: Html::trust(sanitize_html(error).into_owned()),
        }
    }

    fn body_attachment_missing(&self) -> Self::Body {
        PartBody::AttachmentMissing
    }

    fn body_sticker(&self, content: String) -> Self::Body {
        PartBody::Sticker {
            html: Html::trust(content),
        }
    }

    fn body_app(&self, content: String) -> Self::Body {
        PartBody::App {
            html: Html::trust(content),
        }
    }

    fn body_app_error(&self, message: &Message, why: String) -> Self::Body {
        PartBody::AppError {
            html: Html::trust(
                sanitize_html(&format!(
                    "Unable to format {:?} message: {why}",
                    message.variant()
                ))
                .into_owned(),
            ),
        }
    }

    fn body_retracted(&self, content: String) -> Self::Body {
        PartBody::Retracted {
            html: Html::trust(content),
        }
    }

    fn body_escape(&self, text: &str) -> String {
        sanitize_html(text).into_owned()
    }

    fn config(&self) -> &Config {
        self.config
    }
}

/// Truncate a message GUID for human-readable display in forensic-mode
/// tapback bubbles. Keeps the leading segment plus an ellipsis so the
/// bubble stays compact while remaining greppable against the full id.
fn short_guid(guid: &str) -> String {
    const PREFIX_CHARS: usize = 8;
    let prefix: String = guid.chars().take(PREFIX_CHARS).collect();
    if guid.chars().count() <= PREFIX_CHARS {
        prefix
    } else {
        format!("{prefix}…")
    }
}

// MARK: Impl
impl HTML<'_> {
    /// Resolve the message a reply is responding to and build the
    /// view-model fragment shown above the reply in forensic mode. Returns
    /// `None` if the parent isn't in the database (filtered out, or the
    /// reply is an orphan) so the caller can render the reply without a
    /// quote header rather than dropping it.
    fn resolve_replying_to(&self, reply: &Message) -> Option<ReplyingToVM> {
        let orig_guid = reply.thread_originator_guid.as_deref()?;
        let mut parent = Message::from_guid(orig_guid, self.config.data_source.db()).ok()?;
        // iMessage typically stores message text in the attributedBody blob
        // (not the `text` column) and our snippet helper reads `text`
        // directly. Without this call, every snippet falls back to the
        // "[attachment]"/"[no preview]" placeholder.
        apply_body(&mut parent, self.config.data_source.db());
        let sender = self
            .config
            .who(
                parent.handle_id,
                parent.is_from_me(),
                &parent.destination_caller_id,
            )
            .to_string();
        Some(ReplyingToVM {
            sender,
            snippet: build_reply_snippet(&parent),
            anchor_target: parent.guid,
        })
    }

    /// Build the per-message forensic metadata strip rendered at the
    /// bottom of every bubble in `--forensic` mode. Timestamps are emitted
    /// only when the underlying field is non-zero so untracked events
    /// (e.g. a never-read message) leave the strip cleaner rather than
    /// rendering an epoch-of-2001 placeholder.
    fn build_forensic_meta(&self, message: &Message) -> ForensicMetaVM {
        let delivered = (message.date_delivered != 0)
            .then(|| format_timestamp(message.date_delivered, self.config.offset));
        let read = (message.date_read != 0)
            .then(|| format_timestamp(message.date_read, self.config.offset));
        ForensicMetaVM {
            guid_short: short_guid(&message.guid),
            delivered,
            read,
            edited: message.is_edited(),
            deleted: message.is_deleted(),
            chat_id: message.chat_id,
        }
    }

    /// Resolve the message that a tapback was applied to so the bubble can
    /// show "in reaction to <sender>: <snippet>" and link back to the
    /// target's anchor. Returns `None` when the target isn't in the
    /// database (filtered out, recently-deleted, or unparseable
    /// `associated_message_guid`).
    fn resolve_in_reaction_to(&self, tapback_msg: &Message) -> Option<InReactionToVM> {
        let (_idx, target_guid) = tapback_msg.clean_associated_guid()?;
        let mut target = Message::from_guid(target_guid, self.config.data_source.db()).ok()?;
        // Same reason as `resolve_replying_to`: snippet needs `text`
        // materialized from the attributedBody blob.
        apply_body(&mut target, self.config.data_source.db());
        let sender = self
            .config
            .who(
                target.handle_id,
                target.is_from_me(),
                &target.destination_caller_id,
            )
            .to_string();
        Some(InReactionToVM {
            sender,
            snippet: build_reply_snippet(&target),
            anchor_target: target.guid,
        })
    }

    fn get_time(&self, message: &Message) -> (String, String) {
        message_time(self.config, message)
    }

    fn write_headers(file: &mut BufWriter<File>) -> Result<(), RuntimeError> {
        file.write_all(HEADER.as_bytes())?;
        file.write_all(b"<style>\n")?;
        file.write_all(STYLE.as_bytes())?;
        file.write_all(b"\n</style>")?;
        file.write_all(b"<link rel=\"stylesheet\" href=\"style.css\">")?;
        file.write_all(b"\n</head>\n<body>\n")?;
        Ok(())
    }

    fn apply_active_attributes<'a>(
        &'a self,
        text: &'a str,
        active_attrs: &'a [(usize, &[TextEffect])],
    ) -> Cow<'a, str> {
        // The first non-`Default` effect flips us into the owned path; from
        // that point on every iteration reads from `owned` and writes the
        // next render back into it.
        let mut owned: Option<String> = None;
        for (_, effects) in active_attrs {
            for effect in *effects {
                if matches!(effect, TextEffect::Default) {
                    continue;
                }
                let current = owned.as_deref().unwrap_or(text);
                owned = Some(self.format_effect(current, effect).into_owned());
            }
        }

        match owned {
            Some(s) => Cow::Owned(s),
            None => Cow::Borrowed(text),
        }
    }
}

// MARK: Tests

#[cfg(test)]
mod tests {
    use std::{env::current_dir, path::PathBuf};

    use crate::{
        Config, HTML, Options,
        app::{
            compatibility::attachment_manager::AttachmentManagerMode, contacts::Name,
            export_type::ExportType,
        },
        exporters::formatter::{AttachmentRender, MessageFormatter, RenderContext},
    };

    use imessage_database::{
        message_types::text_effects::TextEffect,
        tables::{
            messages::models::{AttachmentMeta, BubbleComponent, TextAttributes},
            table::ME,
        },
        util::{dirs::home, platform::Platform},
    };

    #[test]
    fn can_create() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();
        assert_eq!(exporter.state.files.len(), 0);
    }

    #[test]
    fn can_get_time_valid() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        // let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        // Create fake message
        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;

        assert_eq!(
            exporter.get_time(&message),
            (
                "May 17, 2022  5:29:42 PM".to_string(),
                "(Read by you after 1 hour, 49 seconds)".to_string()
            )
        );
    }

    #[test]
    fn can_get_time_invalid() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        // Create fake message
        let mut message = Config::fake_message();
        // May 17, 2022  9:30:31 PM
        message.date = 674530231992568192;
        // May 17, 2022  9:30:31 PM
        message.date_delivered = 674530231992568192;
        // Wed May 18 2022 02:36:24 GMT+0000
        message.date_read = 674526582885055488;
        assert_eq!(
            exporter.get_time(&message),
            ("May 17, 2022  6:30:31 PM".to_string(), String::new())
        );
    }

    #[test]
    fn can_format_html_from_me_normal() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_message_with_html() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("<table></table>".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">&lt;table&gt;&lt;/table&gt;</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_from_me_normal_deleted() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.text = Some("Hello world".to_string());
        message.date = 674526582885055488;
        message.is_from_me = true;
        message.deleted_from = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        <span class=\"deleted\">This message was deleted from the conversation!</span>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_from_me_normal_read() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_delivered = 674530231992568192;
        message.is_from_me = true;
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                (Read by them after 1 hour, 49 seconds)\n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_from_them_normal() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.handle_id = Some(999999);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Sample Contact</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_from_them_normal_read() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.handle_id = Some(999999);
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                (Read by you after 1 hour, 49 seconds)\n            </span>\n            \n            <span class=\"sender\">Sample Contact</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_from_them_custom_name_read() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.custom_name = Some("Name".to_string());
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.handle_id = Some(999999);
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                (Read by Name after 1 hour, 49 seconds)\n            </span>\n            \n            <span class=\"sender\">Sample Contact</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_shareplay() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.item_type = 6;

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        <span class=\"shareplay\"><hr>SharePlay Message Ended</span>\n        \n        \n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_announcement() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 2;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You named the conversation <b>Hello world</b></p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_announcement_custom_name() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.custom_name = Some("Name".to_string());
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.item_type = 2;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> Name named the conversation <b>Hello world</b></p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn html_announcement_who_is_escaped_once() {
        // Regression: `who` is rendered with the default escaper in the
        // template, so the formatter must not pre-escape it. Pre-escaping
        // would produce `&amp;amp;` for an `&` in the name.
        let mut options = Options::fake_options(ExportType::Html);
        options.custom_name = Some("Bob & <Alice>".to_string());
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.is_from_me = true;
        message.item_type = 3; // ParticipantLeft

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> Bob &amp; &lt;Alice&gt; left the conversation.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_reply_top_level() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "TOP-GUID".to_string();
        message.text = Some("hello".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.thread_originator_guid = Some("ORIG-GUID".to_string());
        message.thread_originator_part = Some("0:0:0".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\" id=\"r-TOP-GUID\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=TOP-GUID\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            \n            <span class=\"reply_anchor\"><a title=\"View in thread\" href=\"#TOP-GUID\">⇱</a></span>\n            \n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">hello</span>\n    </div>\n\n        \n        \n        <span class=\"reply_context\">This message responded to an earlier message.</span>\n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_reply_in_thread() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "INNER-GUID".to_string();
        message.text = Some("hello".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.thread_originator_guid = Some("ORIG-GUID".to_string());
        message.thread_originator_part = Some("0:0:0".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut buf = String::with_capacity(2048);
        exporter
            .format_message_into(&message, RenderContext::Reply, &mut buf)
            .unwrap();
        let actual = buf;
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=INNER-GUID\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            \n            <span class=\"reply_anchor\"><a title=\"View in context\" href=\"#r-INNER-GUID\">⇲</a></span>\n            \n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">hello</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_non_reply_has_no_anchor() {
        // Sanity check: a regular message has no reply anchor and no anchor id.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "PLAIN-GUID".to_string();
        message.text = Some("hello".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=PLAIN-GUID\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">hello</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn short_guid_under_threshold_returned_as_is() {
        assert_eq!(super::short_guid("ABCDEF"), "ABCDEF");
    }

    #[test]
    fn short_guid_at_threshold_returned_as_is() {
        assert_eq!(super::short_guid("ABCDEFGH"), "ABCDEFGH");
    }

    #[test]
    fn short_guid_over_threshold_truncated_with_ellipsis() {
        assert_eq!(
            super::short_guid("ABCDEFGHIJKL-1234"),
            "ABCDEFGH…",
        );
    }

    // MARK: Forensic mode tapback bubble tests

    #[test]
    fn forensic_html_tapback_bubble_added_loved_with_known_target() {
        const TARGET_GUID: &str = "0355C6E1-D0C8-4212-AA87-DD8AE4FD1203";

        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "TAPBACK-GUID-FULL-1234567890".to_string();
        message.associated_message_type = Some(2000); // Added Loved
        message.associated_message_guid = Some(format!("p:0/{TARGET_GUID}"));
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback_bubble(&message).unwrap();

        // Anchors: bubble has its own id, reaction reference points at target
        assert!(
            actual.contains("id=\"TAPBACK-GUID-FULL-1234567890\""),
            "expected bubble id=guid, got: {actual}",
        );
        assert!(
            actual.contains(&format!("href=\"#{TARGET_GUID}\"")),
            "expected reference link to target, got: {actual}",
        );
        // Summary line carries kind + action + sender + timestamp
        assert!(
            actual.contains("Loved"),
            "expected kind to be Loved, got: {actual}",
        );
        assert!(
            actual.contains("Added by"),
            "expected 'Added by' phrasing, got: {actual}",
        );
        assert!(
            actual.contains("Sample Contact"),
            "expected sender 'Sample Contact', got: {actual}",
        );
        assert!(
            actual.contains("May 17, 2022  5:29:42 PM"),
            "expected forensic timestamp, got: {actual}",
        );
        // GUID display present
        assert!(
            actual.contains("TAPBACK-…") || actual.contains("guid: TAPBACK"),
            "expected short guid in bubble, got: {actual}",
        );
        // Removed-class not applied to Added
        assert!(
            !actual.contains("tapback_bubble_removed"),
            "Added tapbacks should not carry the removed class, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_tapback_bubble_removed_loved_carries_removed_class() {
        const TARGET_GUID: &str = "0355C6E1-D0C8-4212-AA87-DD8AE4FD1203";

        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "REMOVED-TAPBACK-GUID".to_string();
        message.associated_message_type = Some(3000); // Removed Loved
        message.associated_message_guid = Some(format!("p:0/{TARGET_GUID}"));
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback_bubble(&message).unwrap();
        assert!(
            actual.contains("tapback_bubble_removed"),
            "Removed tapbacks must carry the removed class, got: {actual}",
        );
        assert!(
            actual.contains("Removed by"),
            "expected 'Removed by' phrasing, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_tapback_bubble_missing_target_omits_reference_header() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "ORPHAN-TAPBACK-GUID".to_string();
        message.associated_message_type = Some(2000); // Added Loved
        message.associated_message_guid = Some("p:0/MISSING-TARGET-GUID-XXXXXXXXXXXXXXXX".to_string());
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback_bubble(&message).unwrap();
        assert!(
            !actual.contains("in_reaction_to"),
            "orphan tapback bubble should omit reference header, got: {actual}",
        );
        // Bubble still renders so the row isn't lost
        assert!(
            actual.contains("id=\"ORPHAN-TAPBACK-GUID\""),
            "orphan tapback bubble must still render with its anchor, got: {actual}",
        );
        assert!(
            actual.contains("Loved"),
            "orphan tapback bubble must still surface kind, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_message_with_tapbacks_does_not_inline_them() {
        // When forensic is on, format_message_into must suppress the inline
        // <div class="tapbacks"> block under a message. The tapbacks are
        // rendered separately as timeline bubbles.
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        // Pre-cache a tapback on this message's guid so build_tapbacks would
        // otherwise emit a tapbacks block. We construct the cache manually
        // because the fixture db's seed data is fixed.
        let host_guid = "HOST-GUID".to_string();
        let mut tb_msg = Config::fake_message();
        tb_msg.guid = "FAKE-TB-GUID".to_string();
        tb_msg.associated_message_type = Some(2000);
        tb_msg.associated_message_guid = Some(format!("p:0/{host_guid}"));
        tb_msg.handle_id = Some(999999);
        let mut idx_map = std::collections::HashMap::new();
        idx_map.insert(0_usize, vec![tb_msg]);
        config.tapbacks.insert(host_guid.clone(), idx_map);

        let exporter = HTML::new(&config).unwrap();

        let mut host = Config::fake_message();
        host.date = 674526582885055488;
        host.guid = host_guid;
        host.text = Some("hi".to_string());
        host.is_from_me = true;
        host.chat_id = Some(0);
        host.generate_text_legacy(config.data_source.db()).unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&host, RenderContext::TopLevel, &mut actual)
            .unwrap();
        assert!(
            !actual.contains("class=\"tapbacks\""),
            "expected no inline tapbacks block in forensic mode, got: {actual}",
        );
    }

    // MARK: Forensic mode per-message metadata strip

    #[test]
    fn forensic_html_message_renders_forensic_meta_with_all_fields() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.date_delivered = 674526582885055488;
        message.date_read = 674530231992568192; // 1h+ later
        message.date_edited = 674530231992568192; // marks is_edited()
        message.guid = "META-FULL-GUID-1234567890".to_string();
        message.text = Some("hi".to_string());
        message.is_from_me = true;
        message.chat_id = Some(42);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        // The strip must include every captured field
        assert!(
            actual.contains("class=\"forensic_meta\""),
            "expected forensic_meta strip in output, got: {actual}",
        );
        assert!(
            actual.contains("guid: META-FUL"),
            "expected truncated guid in strip, got: {actual}",
        );
        assert!(
            actual.contains("delivered: May 17, 2022  5:29:42 PM"),
            "expected delivered timestamp, got: {actual}",
        );
        assert!(
            actual.contains("read: May 17, 2022  6:30:31 PM"),
            "expected read timestamp, got: {actual}",
        );
        assert!(
            actual.contains("edited"),
            "expected edited flag, got: {actual}",
        );
        assert!(
            actual.contains("chat: 42"),
            "expected chat id, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_message_omits_zero_delivery_and_read_fields() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        // date_delivered and date_read left at 0 (default)
        message.guid = "META-MIN-GUID-12345".to_string();
        message.text = Some("hi".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        assert!(
            actual.contains("class=\"forensic_meta\""),
            "expected forensic_meta strip to render even when timestamps are zero, got: {actual}",
        );
        assert!(
            actual.contains("guid: META-MIN"),
            "expected truncated guid, got: {actual}",
        );
        assert!(
            !actual.contains("delivered:"),
            "must NOT render delivered when date_delivered==0, got: {actual}",
        );
        assert!(
            !actual.contains("read:"),
            "must NOT render read when date_read==0, got: {actual}",
        );
        assert!(
            !actual.contains(">edited<"),
            "must NOT render edited flag for un-edited messages, got: {actual}",
        );
    }

    #[test]
    fn default_mode_message_renders_no_forensic_meta() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.date_delivered = 674526582885055488;
        message.date_read = 674530231992568192;
        message.guid = "DEFAULT-MODE-GUID".to_string();
        message.text = Some("hi".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        assert!(
            !actual.contains("forensic_meta"),
            "default mode must not render the forensic metadata strip, got: {actual}",
        );
    }

    // MARK: Forensic mode reply tests

    #[test]
    fn forensic_html_reply_quote_header_carries_parent_text_not_attachment_placeholder() {
        // Regression: resolve_replying_to must apply_body() on the fetched
        // parent or its `text` column stays empty (iMessage stores message
        // text in attributedBody, not the raw text column) and the snippet
        // falls back to "[attachment]" / "[no preview]" instead of showing
        // the actual message that was replied to.
        const PARENT_GUID: &str = "0355C6E1-D0C8-4212-AA87-DD8AE4FD1203";

        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "REPLY-WITH-TEXT".to_string();
        message.text = Some("got it".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.thread_originator_guid = Some(PARENT_GUID.to_string());
        message.thread_originator_part = Some("0:0:0".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        assert!(
            !actual.contains("[attachment]") && !actual.contains("[no preview]"),
            "snippet must show parent's real text, not the placeholder; got: {actual}",
        );
        // The known parent's text in the bundled fixture
        assert!(
            actual.contains("eat as quick as possible"),
            "expected parent message text inside the replying_to header, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_reply_top_level_with_known_parent_renders_quote_header() {
        // Test DB ships with a real message at this GUID; we use it as the
        // synthetic reply's parent so resolve_replying_to can populate the
        // quote header.
        const PARENT_GUID: &str = "0355C6E1-D0C8-4212-AA87-DD8AE4FD1203";

        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "REPLY-GUID".to_string();
        message.text = Some("got it".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.thread_originator_guid = Some(PARENT_GUID.to_string());
        message.thread_originator_part = Some("0:0:0".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        // Forensic mode rules we assert:
        //   - id is the bare guid (no `r-` prefix)
        //   - replying_to div exists with an anchor pointing at the parent
        //   - trailing_reply_context stub is gone
        //   - reply_anchor (the ⇱ icon) is gone (no inline-thread view to link to)
        assert!(
            actual.contains("id=\"REPLY-GUID\""),
            "expected forensic top-level id=guid, got: {actual}",
        );
        assert!(
            actual.contains(&format!("<div class=\"replying_to\"><a href=\"#{PARENT_GUID}\">↪ ")),
            "expected replying_to anchor to parent, got: {actual}",
        );
        assert!(
            !actual.contains("This message responded to an earlier message"),
            "trailing_reply_context stub should be suppressed in forensic mode, got: {actual}",
        );
        assert!(
            !actual.contains("reply_anchor"),
            "reply_anchor link should be suppressed in forensic mode, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_reply_top_level_with_missing_parent_renders_without_quote_header() {
        // GUID is not in the fixture db; resolve_replying_to returns None
        // and the reply must still render (no quote header, no crash).
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "ORPHAN-REPLY-GUID".to_string();
        message.text = Some("got it".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.thread_originator_guid = Some("MISSING-ORIG-GUID".to_string());
        message.thread_originator_part = Some("0:0:0".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        assert!(
            actual.contains("id=\"ORPHAN-REPLY-GUID\""),
            "expected forensic top-level id=guid even for orphans, got: {actual}",
        );
        assert!(
            !actual.contains("replying_to"),
            "no replying_to header when parent is missing, got: {actual}",
        );
        assert!(
            actual.contains("got it"),
            "reply body must still render when parent missing, got: {actual}",
        );
    }

    #[test]
    fn forensic_html_non_reply_top_level_gets_bare_guid_anchor() {
        // Every top-level message in forensic mode needs an anchor so
        // future replies can link back to it.
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.guid = "FORENSIC-PLAIN-GUID".to_string();
        message.text = Some("hi".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        assert!(
            actual.contains("id=\"FORENSIC-PLAIN-GUID\""),
            "expected forensic top-level id=guid on non-reply, got: {actual}",
        );
        assert!(
            !actual.contains("replying_to"),
            "non-reply must not render a replying_to header, got: {actual}",
        );
    }

    #[test]
    fn can_format_html_part_body_attachment_missing_standalone() {
        // BubbleComponent::Attachment with no matching Attachment row →
        // PartBody::AttachmentMissing → "<span class=\"attachment_error\">Attachment does not exist!</span>"
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.rowid = i32::MAX; // unlikely to exist in fixture db
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.components = vec![BubbleComponent::Attachment(AttachmentMeta::default())];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"attachment_error\">Attachment does not exist!</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_url_message_without_payload_uses_text_fallback() {
        // Defensive path in dispatch_app_balloon: when a URL-balloon message
        // has no payload row but does carry `text`, the normal `format_url`
        // pipeline still produces a clickable link via its msg.text fallback.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.rowid = i32::MAX; // not in fixture db, so payload_data returns None
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.balloon_bundle_id = Some("com.apple.messages.URLBalloonProvider".to_string());
        message.text = Some("https://example.com".to_string());
        message.components = vec![BubbleComponent::App];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <div class=\"app\"><a href=\"https://example.com\"><div class=\"app_header\"><div class=\"name\">https://example.com</div></div></a></div>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_url_message_without_payload_escapes_text() {
        // The fallback flows msg.text through `format_url` and the Askama
        // template's auto-escaper; raw HTML in `text` must not survive.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.rowid = i32::MAX;
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.balloon_bundle_id = Some("com.apple.messages.URLBalloonProvider".to_string());
        message.text = Some("https://x.test/?q=<script>".to_string());
        message.components = vec![BubbleComponent::App];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <div class=\"app\"><a href=\"https://x.test/?q=&lt;script&gt;\"><div class=\"app_header\"><div class=\"name\">https://x.test/?q=&lt;script&gt;</div></div></a></div>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn expressive_renders_via_display_impl() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.expressive_send_style_id =
            Some("com.apple.messages.effect.CKConfettiEffect".to_string());
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hello world</span>\n    </div>\n<span class=\"expressive\">Sent with Confetti</span>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn expressive_empty_unknown_renders_like_none() {
        // expressive_send_style_id = Some("") rows must render identically to
        // expressive_send_style_id = None: no stray empty `<span class="expressive">`
        // from the empty Unknown variant passing through the template's `Some` guard.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let build = |expressive: Option<String>| {
            let mut m = Config::fake_message();
            m.date = 674526582885055488;
            m.text = Some("Hello world".to_string());
            m.is_from_me = true;
            m.chat_id = Some(0);
            m.expressive_send_style_id = expressive;
            m.generate_text_legacy(config.data_source.db()).unwrap();
            m
        };

        let mut baseline = String::new();
        exporter
            .format_message_into(&build(None), RenderContext::TopLevel, &mut baseline)
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(
                &build(Some(String::new())),
                RenderContext::TopLevel,
                &mut actual,
            )
            .unwrap();

        assert_eq!(actual, baseline);
    }

    #[test]
    fn can_format_html_part_body_app_error_on_normal_variant() {
        // BubbleComponent::App on a Variant::Normal message → format_app
        // returns WrongMessageType → PartBody::AppError, escaped via sanitize_html.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.rowid = i32::MAX;
        message.is_from_me = true;
        message.chat_id = Some(0);
        // Default fake_message is Variant::Normal (no balloon_bundle_id, AMT=0).
        // Adding a BubbleComponent::App forces format_app's else-branch.
        message.components = vec![BubbleComponent::App];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <div class=\"app_error\">Unable to format Normal message: Failed to parse property list: Message is not an app message!</div>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn format_message_into_appends_to_existing_buffer() {
        // Mirrors the production hot path in `run_export`, which reuses a
        // single `String` across messages via `clear()` + `format_message_into`.
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.text = Some("hello".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut standalone = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut standalone)
            .unwrap();

        // Pre-fill the buffer with content the writer would have left in
        // (e.g. the previous message). format_message_into should leave that
        // content alone and append the new render after it.
        let prefix = "<!-- previous message -->\n";
        let mut buf = String::with_capacity(2048);
        buf.push_str(prefix);
        let cap_before = buf.capacity();

        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut buf)
            .unwrap();

        assert!(
            buf.starts_with(prefix),
            "format_message_into must not overwrite existing buffer content"
        );
        assert_eq!(&buf[prefix.len()..], standalone);
        // Capacity should not have shrunk; if anything it grows to fit the
        // new content.
        assert!(buf.capacity() >= cap_before);
    }

    #[test]
    fn can_format_html_announcement_unknown() {
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected =
            "\n<div class=\"announcement\">\n    <p>Unable to format announcement!</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_removed() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.participants.insert(1, Name::fake_name("Other"));
        config.real_participants.insert(0, 0);
        config.real_participants.insert(1, 1);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 1;
        message.group_action_type = 1;
        message.other_handle = Some(1);

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You removed Other from the conversation.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_removed_other() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.participants.insert(1, Name::fake_name("Other"));
        config.participants.insert(2, Name::fake_name("Second"));
        config.real_participants.insert(0, 0);
        config.real_participants.insert(1, 1);
        config.real_participants.insert(2, 2);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = false;
        message.handle_id = Some(1);
        message.item_type = 1;
        message.group_action_type = 1;
        message.other_handle = Some(2);

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> Other removed Second from the conversation.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_changed_number() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.participants.insert(1, Name::fake_name("Other"));
        config.real_participants.insert(0, 0);
        config.real_participants.insert(1, 1);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = false;
        message.handle_id = Some(1);
        message.item_type = 1;
        message.group_action_type = 0;
        message.other_handle = Some(1);

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> Other changed their phone number.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_added() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.participants.insert(1, Name::fake_name("Other"));
        config.real_participants.insert(0, 0);
        config.real_participants.insert(1, 1);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 1;
        message.group_action_type = 0;
        message.other_handle = Some(1);

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You added Other to the conversation.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_left() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 3;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You left the conversation.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_icon_removed() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 3;
        message.group_action_type = 2;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You removed the group photo.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_group_icon_added() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 3;
        message.group_action_type = 1;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You changed the group photo.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_chat_background_removed() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 3;
        message.group_action_type = 6;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You removed the chat background.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_chat_background_added() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 3;
        message.group_action_type = 4;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You changed the chat background.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_audio_message_kept() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.is_from_me = true;
        message.item_type = 5;

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "\n<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You kept an audio message.</p>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_me() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        config.real_participants.insert(0, 0);

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2000);
        message.associated_message_guid = Some("fake_guid".to_string());

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\"><b>Loved</b> by Me</span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_them() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2000);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\"><b>Loved</b> by Sample Contact</span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_custom_emoji() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2006);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.associated_message_emoji = Some("☕️".to_string());

        let actual = exporter.format_tapback(&message).unwrap();
        // The result contains `&nbsp;`
        let expected = "<span class=\"tapback\"><b>☕\u{fe0f}</b> by Sample Contact</span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_custom_sticker() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.num_attachments = 1;

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\">Sticker from Sample Contact not found!</span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_custom_sticker_exists() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.num_attachments = 1;
        message.rowid = 452567;

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = format!(
            "<img src=\"{}/Library/Messages/StickerCache/8e682c381ab52ec2-289D9E83-33EE-4153-AF13-43DB31792C6F/289D9E83-33EE-4153-AF13-43DB31792C6F.heic\" loading=\"lazy\">\n<div class=\"sticker_name\">App: Free People</div><div class=\"sticker_tapback\">&nbsp;by Sample Contact</div>",
            home()
        );

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_tapback_custom_sticker_removed() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(3007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.num_attachments = 1;
        message.rowid = 452567;

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "";

        assert_eq!(actual, expected);
    }

    // MARK: Forensic mode tapback tests

    #[test]
    fn forensic_html_tapback_added_reaction_includes_timestamp() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(2000);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\"><b>Loved</b> by Sample Contact<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn forensic_html_tapback_removed_reaction_renders_with_timestamp() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(3000);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback tapback_removed\"><b>Loved</b> removed by Sample Contact<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn forensic_html_tapback_added_custom_emoji_includes_timestamp() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(2006);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.associated_message_emoji = Some("☕️".to_string());

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\"><b>☕\u{fe0f}</b> by Sample Contact<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn forensic_html_tapback_removed_custom_emoji_renders() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(3006);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.associated_message_emoji = Some("☕️".to_string());

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback tapback_removed\"><b>☕\u{fe0f}</b> removed by Sample Contact<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn forensic_html_tapback_removed_sticker_renders_textually() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(3007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.num_attachments = 1;
        message.rowid = 452567;

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback tapback_removed\"><b>Sticker</b> removed by Sample Contact<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn forensic_html_tapback_added_sticker_missing_includes_timestamp() {
        let mut options = Options::fake_options(ExportType::Html);
        options.forensic = true;
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999999, 999999);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.associated_message_type = Some(2007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.num_attachments = 1;

        let actual = exporter.format_tapback(&message).unwrap();
        let expected = "<span class=\"tapback\">Sticker from Sample Contact not found!<div class=\"tapback_time\">May 17, 2022  5:29:42 PM</div></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_started_sharing_location_me() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.is_from_me = false;
        message.other_handle = Some(2);
        message.share_status = false;
        message.share_direction = Some(false);
        message.item_type = 4;

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">Dec 31, 2000  4:00:00 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        <span class=\"shared_location\"><hr>Started sharing location!</span>\n        \n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_stopped_sharing_location_me() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.is_from_me = false;
        message.other_handle = Some(2);
        message.share_status = true;
        message.share_direction = Some(false);
        message.item_type = 4;

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">Dec 31, 2000  4:00:00 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        <span class=\"shared_location\"><hr>Stopped sharing location!</span>\n        \n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_started_sharing_location_them() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.handle_id = None;
        message.is_from_me = false;
        message.other_handle = Some(0);
        message.share_status = false;
        message.share_direction = Some(false);
        message.item_type = 4;

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">Dec 31, 2000  4:00:00 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Unknown</span>\n        </p>\n        \n        \n        \n        \n        <span class=\"shared_location\"><hr>Started sharing location!</span>\n        \n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_stopped_sharing_location_them() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.handle_id = None;
        message.is_from_me = false;
        message.other_handle = Some(0);
        message.share_status = true;
        message.share_direction = Some(false);
        message.item_type = 4;

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">Dec 31, 2000  4:00:00 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Unknown</span>\n        </p>\n        \n        \n        \n        \n        <span class=\"shared_location\"><hr>Stopped sharing location!</span>\n        \n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_attachment_macos() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(
            actual,
            AttachmentRender::Embedded("<img src=\"a/b/c/d.jpg\" loading=\"lazy\">".to_string())
        );
    }

    #[test]
    fn can_format_html_attachment_macos_invalid_disabled() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.filename = None;
        attachment.transfer_name = None;

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(actual, AttachmentRender::MissingFilename);
    }

    #[test]
    fn can_format_html_attachment_macos_invalid_clone() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.attachment_manager.mode = AttachmentManagerMode::Clone;

        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.filename = None;
        attachment.transfer_name = None;

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(actual, AttachmentRender::MissingFilename);
    }

    #[test]
    fn can_format_html_attachment_ios() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let mut config = Config::fake_app(options);
        config.options.no_lazy = true;
        config.options.platform = Platform::iOS;
        let exporter = HTML::new(&config).unwrap();
        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();

        let AttachmentRender::Embedded(actual) =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default())
        else {
            panic!("expected AttachmentRender::Embedded");
        };

        assert!(actual.ends_with("33/33c81da8ae3194fc5a0ea993ef6ffe0b048baedb\">"));
    }

    #[test]
    fn can_format_html_attachment_ios_invalid_disabled() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.filename = None;
        attachment.transfer_name = None;

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(actual, AttachmentRender::MissingFilename);
    }

    #[test]
    fn can_format_html_attachment_ios_invalid_clone() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.attachment_manager.mode = AttachmentManagerMode::Clone;

        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.filename = None;
        attachment.transfer_name = None;

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(actual, AttachmentRender::MissingFilename);
    }

    #[test]
    fn can_format_html_attachment_folder() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        let folder_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/");
        attachment.mime_type = None;
        attachment.transfer_name = Some("test_data".to_string());
        attachment.copied_path = Some(folder_path);

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        let abs_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/");
        let expected = format!(
            "<p>\n    Folder: <i>test_data</i> (100.00 B)\n    <a href=\"{}\">Click to open</a>\n</p>",
            abs_path.display()
        );

        assert_eq!(actual, AttachmentRender::Embedded(expected));
    }

    #[test]
    fn can_format_html_attachment_text_download() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        // text/* → MediaType::Text(_) → AttachmentVariant::Download
        attachment.mime_type = Some("text/plain".to_string());
        attachment.filename = Some("notes.txt".to_string());
        attachment.transfer_name = Some("notes.txt".to_string());

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(
            actual,
            AttachmentRender::Embedded(
                "<a href=\"notes.txt\">Click to download notes.txt (100.00 B)</a>".to_string()
            )
        );
    }

    #[test]
    fn can_format_html_attachment_application_download() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        // application/* → MediaType::Application(_) → AttachmentVariant::Download
        attachment.mime_type = Some("application/pdf".to_string());
        attachment.filename = Some("doc.pdf".to_string());
        attachment.transfer_name = Some("doc.pdf".to_string());

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(
            actual,
            AttachmentRender::Embedded(
                "<a href=\"doc.pdf\">Click to download doc.pdf (100.00 B)</a>".to_string()
            )
        );
    }

    #[test]
    fn can_format_html_attachment_other_media_type() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        // mime_type without a recognized prefix maps to MediaType::Other(full).
        attachment.mime_type = Some("model/gltf-binary".to_string());
        attachment.filename = Some("scene.glb".to_string());

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(
            actual,
            AttachmentRender::Embedded(
                "<p>Unable to embed model/gltf-binary attachments: scene.glb</p>".to_string()
            )
        );
    }

    #[test]
    fn can_format_html_attachment_unknown() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        let folder_path = "Fake";
        attachment.mime_type = None;
        attachment.transfer_name = Some("test_data".to_string());
        attachment.copied_path = Some(PathBuf::from(folder_path));

        let actual =
            exporter.format_attachment(&mut attachment, &message, &AttachmentMeta::default());

        assert_eq!(
            actual,
            AttachmentRender::Embedded(
                "<p>Unknown attachment type: Fake</p>\n<a href=\"Fake\">Download (100.00 B)</a>"
                    .to_string()
            )
        );
    }

    #[test]
    fn can_format_html_attachment_sticker() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.export_path = current_dir().unwrap().parent().unwrap().to_path_buf();

        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.rowid = 3;
        attachment.is_sticker = true;
        let sticker_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/stickers/outline.heic");
        attachment.filename = Some(sticker_path.to_string_lossy().to_string());
        attachment.copied_path = Some(sticker_path);

        let actual = exporter.format_sticker(&mut attachment, &message);

        assert_eq!(
            actual,
            "<img src=\"imessage-database/test_data/stickers/outline.heic\" loading=\"lazy\">\n<div class=\"sticker_effect\">Sent with Outline effect</div>"
        );

        // Remove the file created by the constructor for this test
        let orphaned_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("orphaned.html");
        let _ = std::fs::remove_file(orphaned_path);
    }

    #[test]
    fn can_format_html_attachment_sticker_genmoji() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.export_path = current_dir().unwrap().parent().unwrap().to_path_buf();

        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.rowid = 2;
        attachment.is_sticker = true;
        let sticker_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/stickers/outline.heic");
        attachment.filename = Some(sticker_path.to_string_lossy().to_string());
        attachment.copied_path = Some(sticker_path);
        attachment.emoji_description = Some("pink poodle".to_string());

        let actual = exporter.format_sticker(&mut attachment, &message);

        assert_eq!(
            actual,
            "<img src=\"imessage-database/test_data/stickers/outline.heic\" loading=\"lazy\">\n<div class=\"genmoji_prompt\">Genmoji prompt: pink poodle</div>"
        );

        // Remove the file created by the constructor for this test
        let orphaned_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("orphaned.html");
        let _ = std::fs::remove_file(orphaned_path);
    }

    #[test]
    fn can_format_html_attachment_sticker_app() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.export_path = current_dir().unwrap().parent().unwrap().to_path_buf();

        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.rowid = 1;
        attachment.is_sticker = true;
        let sticker_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/stickers/outline.heic");
        attachment.filename = Some(sticker_path.to_string_lossy().to_string());
        attachment.copied_path = Some(sticker_path);

        let actual = exporter.format_sticker(&mut attachment, &message);

        assert_eq!(
            actual,
            "<img src=\"imessage-database/test_data/stickers/outline.heic\" loading=\"lazy\">\n<div class=\"sticker_name\">App: Free People</div>"
        );

        // Remove the file created by the constructor for this test
        let orphaned_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("orphaned.html");
        let _ = std::fs::remove_file(orphaned_path);
    }

    #[test]
    fn can_format_html_attachment_audio_transcript() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let message = Config::fake_message();

        let mut attachment = Config::fake_attachment();
        attachment.uti = Some("com.apple.coreaudio-format".to_string());
        attachment.transfer_name = Some("Audio Message.caf".to_string());
        attachment.filename = Some("Audio Message.caf".to_string());
        attachment.mime_type = None;

        let meta = AttachmentMeta {
            transcription: Some("Test".to_string()),
            ..Default::default()
        };

        let actual = exporter.format_attachment(&mut attachment, &message, &meta);

        assert_eq!(
            actual,
            AttachmentRender::Embedded(
                "<div>\n    <audio controls src=\"Audio Message.caf\" type=\"x-caf; codecs=opus\"> </audio>\n</div>\n<hr>\n<span class=\"transcription\">Transcription: Test</span>".to_string()
            )
        );
    }

    #[test]
    fn can_format_html_single_url_no_bundle_id() {
        // Create exporter
        let options = Options::fake_options(ExportType::Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();

        // Use test message payload from test database
        message.guid = "FAKEGUID-D0C8-4212-AA87-DD8AE4FD1203".to_string();
        message.rowid = 123445;

        message.date = 674526582885055488;
        // Set the message components to a single url
        message.text = Some("https://example.com".to_string());
        message.components = vec![BubbleComponent::Text(vec![
                TextAttributes::new(
                    0,
                    84,
                    vec![
                        TextEffect::Link("https://www.ghacks.net/2020/01/23/lastpass-no-longer-listed-on-the-chrome-web-store/".to_string()),
                    ]
                ),
            ]),];

        let body = message.parse_body(config.data_source.db()).unwrap();
        message.apply_body(body);

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();

        assert_eq!(
            actual,
            "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=FAKEGUID-D0C8-4212-AA87-DD8AE4FD1203\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Unknown</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <div class=\"app\"><a href=\"https://www.ghacks.net/2020/01/23/lastpass-no-longer-listed-on-the-chrome-web-store/\"><div class=\"app_header\"><img src=\"https://www.ghacks.net/wp-content/uploads/2020/01/lastpass-chrome-extension.png\" loading=\"lazy\" onerror=\"this.style.display='none'\"><div class=\"name\">gHacks Technology News</div></div><div class=\"app_footer\"><div class=\"caption\">LastPass no longer listed on the Chrome Web Store - gHacks Tech News</div><div class=\"subcaption\">LastPass customers and new users searching for password managers on Google&apos;s Chrome Web Store may have noticed that the LastPass extension for Google Chrome is currently no longer listed on the store.</div></div></a></div>\n    </div>\n\n        \n        \n    </div>\n</div>\n"
        );
    }

    #[test]
    fn can_format_html_translated_message() {
        // Create exporter
        let mut options = Options::fake_options(ExportType::Html);
        options.attachment_manager.mode = AttachmentManagerMode::Clone;

        let mut config = Config::fake_app(options);
        config
            .translated_messages
            .insert("56FE94B9-2345-4A3C-A57F-949BDDDDF9FF".to_string());

        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.guid = "56FE94B9-2345-4A3C-A57F-949BDDDDF9FF".to_string();
        message.rowid = 548216;
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"received\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=56FE94B9-2345-4A3C-A57F-949BDDDDF9FF\">Dec 31, 2000  4:00:00 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Unknown</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Oh, il a traduit ce que j&apos;ai envoyé !</span>\n    <div class=\"translated\"><span class=\"bubble\">Oh it translated what I sent!</span></div>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod balloon_format_tests {
    use std::{collections::HashMap, env::current_dir, fs::File, io::Read};

    use crate::{
        Config, HTML, Options, app::export_type::ExportType::Html,
        exporters::formatter::BalloonFormatter,
    };
    use imessage_database::message_types::{
        app::AppMessage,
        app_store::AppStoreMessage,
        collaboration::CollaborationMessage,
        digital_touch::{DigitalTouch, from_payload as digital_touch_from_payload},
        handwriting::HandwrittenMessage,
        music::MusicMessage,
        placemark::{Placemark, PlacemarkMessage},
        polls::{Poll, PollOption, PollOptionID, PollVote},
        url::URLMessage,
    };

    #[test]
    fn can_format_html_url() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = URLMessage {
            title: Some("title"),
            summary: Some("summary"),
            url: Some("url"),
            original_url: Some("original_url"),
            item_type: Some("item_type"),
            images: vec!["images"],
            icons: vec!["icons"],
            site_name: Some("site_name"),
            placeholder: false,
        };

        let actual = exporter.format_url(&Config::fake_message(), &balloon);
        let expected = "<a href=\"url\"><div class=\"app_header\"><img src=\"images\" loading=\"lazy\" onerror=\"this.style.display='none'\"><div class=\"name\">site_name</div></div><div class=\"app_footer\"><div class=\"caption\">title</div><div class=\"subcaption\">summary</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_url_no_lazy() {
        // Create exporter
        let mut options = Options::fake_options(Html);
        options.no_lazy = true;
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = URLMessage {
            title: Some("title"),
            summary: Some("summary"),
            url: Some("url"),
            original_url: Some("original_url"),
            item_type: Some("item_type"),
            images: vec!["images"],
            icons: vec!["icons"],
            site_name: Some("site_name"),
            placeholder: false,
        };

        let actual = exporter.format_url(&Config::fake_message(), &balloon);
        let expected = "<a href=\"url\"><div class=\"app_header\"><img src=\"images\" onerror=\"this.style.display='none'\"><div class=\"name\">site_name</div></div><div class=\"app_footer\"><div class=\"caption\">title</div><div class=\"subcaption\">summary</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_music() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = MusicMessage {
            url: Some("url"),
            preview: Some("preview"),
            artist: Some("artist"),
            album: Some("album"),
            track_name: Some("track_name"),
            lyrics: None,
        };

        let actual = exporter.format_music(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">track_name</div><audio controls src=\"preview\"> </audio></div><a href=\"url\"><div class=\"app_footer\"><div class=\"caption\">artist</div><div class=\"subcaption\">album</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_music_lyrics() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = MusicMessage {
            url: Some("url"),
            preview: None,
            artist: Some("artist"),
            album: Some("album"),
            track_name: Some("track_name"),
            lyrics: Some(vec!["a", "b"]),
        };

        let actual = exporter.format_music(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">track_name</div><div class=\"ldtext\"><p>a</p><p>b</p></div></div><a href=\"url\"><div class=\"app_footer\"><div class=\"caption\">artist</div><div class=\"subcaption\">album</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn music_balloon_skips_empty_string_fields() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = MusicMessage {
            url: Some("url"),
            preview: None,
            artist: Some(""),
            album: Some(""),
            track_name: Some("track_name"),
            lyrics: None,
        };

        let actual = exporter.format_music(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">track_name</div></div><a href=\"url\"></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_collaboration() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = CollaborationMessage {
            original_url: Some("original_url"),
            url: Some("url"),
            title: Some("title"),
            creation_date: Some(0.),
            bundle_id: Some("bundle_id"),
            app_name: Some("app_name"),
        };

        let actual = exporter.format_collaboration(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">app_name</div></div><a href=\"url\"><div class=\"app_footer\"><div class=\"caption\">title</div><div class=\"subcaption\">url</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_apple_pay() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let actual = exporter.format_apple_pay(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">app_name</div>\n</div><div class=\"app_footer\">\n    <div class=\"caption\">ldtext</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn apple_pay_balloon_emits_nothing_when_both_fields_missing() {
        // Apple Pay balloons with no `app_name` and no `ldtext` must render
        // nothing. `.app_footer` has a grey background + borders in style.css,
        // so an empty wrapper would render as a visible bordered strip.
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: None,
            title: None,
            subtitle: None,
            caption: None,
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: None,
            ldtext: None,
        };

        let actual = exporter.format_apple_pay(&balloon);
        assert_eq!(actual, "");
    }

    #[test]
    fn can_format_html_fitness() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let actual = exporter.format_fitness(&balloon);
        let expected = "<a href=\"url\"><div class=\"app_header\"><img src=\"image\"><div class=\"name\">app_name</div><div class=\"image_title\">title</div><div class=\"image_subtitle\">subtitle</div><div class=\"ldtext\">ldtext</div></div><div class=\"app_footer\"><div class=\"caption\">caption</div><div class=\"subcaption\">subcaption</div><div class=\"trailing_caption\">trailing_caption\n        </div><div class=\"trailing_subcaption\">trailing_subcaption</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_slideshow() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let actual = exporter.format_slideshow(&balloon);
        let expected = "<a href=\"url\"><div class=\"app_header\"><img src=\"image\"><div class=\"name\">app_name</div><div class=\"image_title\">title</div><div class=\"image_subtitle\">subtitle</div><div class=\"ldtext\">ldtext</div></div><div class=\"app_footer\"><div class=\"caption\">caption</div><div class=\"subcaption\">subcaption</div><div class=\"trailing_caption\">trailing_caption\n        </div><div class=\"trailing_subcaption\">trailing_subcaption</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_find_my() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let actual = exporter.format_find_my(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">app_name</div>\n</div><div class=\"app_footer\">\n    <div class=\"caption\">ldtext</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn find_my_balloon_emits_nothing_when_both_fields_missing() {
        // An empty Find My payload must not render bare `.app_header` /
        // `.app_footer` wrappers, which would show as a styled grey strip
        // with no content.
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: None,
            title: None,
            subtitle: None,
            caption: None,
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: None,
            ldtext: None,
        };

        let actual = exporter.format_find_my(&balloon);
        assert_eq!(actual, "");
    }

    #[test]
    fn can_format_html_check_in_timer() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Timer Started"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Timer Started"),
        };

        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check&nbsp;In</div><div class=\"ldtext\">Check&nbsp;In: Timer Started</div></div><div class=\"app_footer\">\n    <div class=\"caption\">Checked in at Oct 14, 2023  1:54:29 PM</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_check_in_timer_late() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Has not checked in when expected, location shared"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Has not checked in when expected, location shared"),
        };

        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check&nbsp;In</div><div class=\"ldtext\">Check&nbsp;In: Has not checked in when expected, location shared</div></div><div class=\"app_footer\">\n    <div class=\"caption\">Checked in at Oct 14, 2023  1:54:29 PM</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_accepted_check_in() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Fake Location"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Fake Location"),
        };

        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check&nbsp;In</div><div class=\"ldtext\">Check&nbsp;In: Fake Location</div></div><div class=\"app_footer\">\n    <div class=\"caption\">Checked in at Oct 14, 2023  1:54:29 PM</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_app_store() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppStoreMessage {
            url: Some("url"),
            app_name: Some("app_name"),
            original_url: Some("original_url"),
            description: Some("description"),
            platform: Some("platform"),
            genre: Some("genre"),
        };

        let actual = exporter.format_app_store(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">app_name</div></div><a href=\"url\"><div class=\"app_footer\"><div class=\"caption\">description</div><div class=\"subcaption\">platform</div><div class=\"trailing_subcaption\">genre</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_placemark() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = PlacemarkMessage {
            url: Some("url"),
            original_url: Some("original_url"),
            place_name: Some("Name"),
            placemark: Placemark {
                name: Some("name"),
                address: Some("address"),
                state: Some("state"),
                city: Some("city"),
                iso_country_code: Some("iso_country_code"),
                postal_code: Some("postal_code"),
                country: Some("country"),
                street: Some("street"),
                sub_administrative_area: Some("sub_administrative_area"),
                sub_locality: Some("sub_locality"),
            },
        };

        let actual = exporter.format_placemark(&balloon);
        let expected = "<a href=\"url\"><div class=\"app_header\"><div class=\"name\">Name</div><div class=\"image_title\">name</div></div><div class=\"app_footer\"><div class=\"caption\">address</div><div class=\"trailing_caption\">postal_code</div><div class=\"subcaption\">country</div><div class=\"trailing_subcaption\">sub_administrative_area</div><div class=\"street\">street</div><div class=\"city\">city</div><div class=\"state\">state</div><div class=\"sub_locality\">sub_locality</div><div class=\"iso_country_code\">iso_country_code</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_poll() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut poll_options: HashMap<PollOptionID, PollOption> = HashMap::new();

        let id1: PollOptionID = "1".to_string();
        let id2: PollOptionID = "2".to_string();
        let id3: PollOptionID = "3".to_string();

        poll_options.insert(
            id1.clone(),
            PollOption {
                text: "Rust".to_string(),
                creator: "alice".to_string(),
                votes: vec![PollVote {
                    voter: "carol".to_string(),
                    option_id: id1.clone(),
                }],
            },
        );

        poll_options.insert(
            id2.clone(),
            PollOption {
                text: "Go".to_string(),
                creator: "bob".to_string(),
                votes: vec![
                    PollVote {
                        voter: "alice".to_string(),
                        option_id: id2.clone(),
                    },
                    PollVote {
                        voter: "bob".to_string(),
                        option_id: id2.clone(),
                    },
                ],
            },
        );

        poll_options.insert(
            id3.clone(),
            PollOption {
                text: "Python".to_string(),
                creator: "carol".to_string(),
                votes: vec![PollVote {
                    voter: "dave".to_string(),
                    option_id: id3.clone(),
                }],
            },
        );

        let poll = Poll {
            options: poll_options,
            order: vec![id1, id2, id3],
        };

        let actual = exporter.format_poll(&poll);
        let expected = "<div class=\"poll-container\"><div class=\"poll-option\">\n        <div class=\"option-header\"><span>Rust</span><span class=\"vote-count\">1</span>\n        </div>\n        <div class=\"vote-bar-container\">\n            <div class=\"vote-bar\" style=\"width: 50%;\"></div>\n        </div><div class=\"voters-list\"><span class=\"voter\">carol</span></div></div><div class=\"poll-option\">\n        <div class=\"option-header\"><span>Go</span><span class=\"vote-count\">2</span>\n        </div>\n        <div class=\"vote-bar-container\">\n            <div class=\"vote-bar\" style=\"width: 100%;\"></div>\n        </div><div class=\"voters-list\"><span class=\"voter\">alice</span><span class=\"voter\">bob</span></div></div><div class=\"poll-option\">\n        <div class=\"option-header\"><span>Python</span><span class=\"vote-count\">1</span>\n        </div>\n        <div class=\"vote-bar-container\">\n            <div class=\"vote-bar\" style=\"width: 50%;\"></div>\n        </div><div class=\"voters-list\"><span class=\"voter\">dave</span></div></div></div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_generic_app() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let actual = exporter.format_generic_app(
            &balloon,
            "bundle_id",
            &mut vec![],
            &Config::fake_message(),
        );
        let expected = "<a href=\"url\"><div class=\"app_header\"><img src=\"image\"><div class=\"name\">app_name</div><div class=\"image_title\">title</div><div class=\"image_subtitle\">subtitle</div><div class=\"ldtext\">ldtext</div></div><div class=\"app_footer\"><div class=\"caption\">caption</div><div class=\"subcaption\">subcaption</div><div class=\"trailing_caption\">trailing_caption\n        </div><div class=\"trailing_subcaption\">trailing_subcaption</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_digital_touch_kiss() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let msg = Config::fake_message();
        let actual = exporter.format_digital_touch(&msg, &DigitalTouch::Kiss);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Digital Touch Message</div>\n</div>\n<div class=\"app_footer\">\n    <div class=\"caption\">Kiss</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_digital_touch_from_payload() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let payload_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/digital_touch_message/sketch.bin");
        let mut payload = vec![];
        File::open(payload_path)
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        let touch = digital_touch_from_payload(&payload).unwrap();

        let msg = Config::fake_message();
        let actual = exporter.format_digital_touch(&msg, &touch);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Digital Touch Message</div>\n</div>\n<div class=\"app_footer\">\n    <div class=\"caption\">Sketch</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_handwriting() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let payload_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/handwritten_message/handwriting.bin");
        let mut payload = vec![];
        File::open(payload_path)
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        let balloon = HandwrittenMessage::from_payload(&payload).unwrap();

        let mut expected = String::new();
        let expected_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/handwritten_message/handwriting.svg");
        File::open(expected_path)
            .unwrap()
            .read_to_string(&mut expected)
            .unwrap();

        let msg = Config::fake_message();
        let actual = exporter.format_handwriting(&msg, &balloon);

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_check_in_estimated_end_time() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&estimatedEndTime=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Timer Started"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Timer Started"),
        };

        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check In</div><div class=\"ldtext\">Check In: Timer Started</div></div><div class=\"app_footer\">\n    <div class=\"caption\">Expected at Oct 14, 2023  1:54:29 PM</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_check_in_trigger_time() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&triggerTime=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Timer Started"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Timer Started"),
        };

        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check In</div><div class=\"ldtext\">Check In: Timer Started</div></div><div class=\"app_footer\">\n    <div class=\"caption\">Was expected at Oct 14, 2023  1:54:29 PM</div>\n</div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_check_in_no_recognized_metadata() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1"),
            title: None,
            subtitle: None,
            caption: Some("Check In"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In"),
        };

        // Without any of the three recognized timestamp keys the footer is
        // omitted entirely (CheckInVM.footer = None → check_in.html drops the
        // `<div class="app_footer">` block).
        let actual = exporter.format_check_in(&balloon);
        let expected = "<div class=\"app_header\">\n    <div class=\"name\">Check In</div><div class=\"ldtext\">Check In</div></div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_poll_empty_options() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        // Empty poll: `order` is empty, so max_votes = 0 (the unwrap_or guard
        // protects bar_width's `checked_div(0)` from panicking even though the
        // for-loop never executes).
        let poll = Poll {
            options: HashMap::new(),
            order: vec![],
        };

        let actual = exporter.format_poll(&poll);
        let expected = "<div class=\"poll-container\"></div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_poll_option_with_zero_votes() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut poll_options: HashMap<PollOptionID, PollOption> = HashMap::new();
        let id: PollOptionID = "1".to_string();
        poll_options.insert(
            id.clone(),
            PollOption {
                text: "Rust".to_string(),
                creator: "alice".to_string(),
                votes: vec![],
            },
        );

        let poll = Poll {
            options: poll_options,
            order: vec![id],
        };

        let actual = exporter.format_poll(&poll);
        let expected = "<div class=\"poll-container\"><div class=\"poll-option\">\n        <div class=\"option-header\"><span>Rust</span><span class=\"vote-count\">0</span>\n        </div>\n        <div class=\"vote-bar-container\">\n            <div class=\"vote-bar\" style=\"width: 0%;\"></div>\n        </div></div></div>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_url_no_site_name_falls_back_to_url() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = URLMessage {
            title: None,
            summary: None,
            url: Some("https://example.com"),
            original_url: None,
            item_type: None,
            images: vec![],
            icons: vec![],
            site_name: None,
            placeholder: false,
        };

        // No images → no <img>; no site_name → name falls back to balloon.url.
        // No title or summary → <div class="app_footer"> block is dropped.
        let actual = exporter.format_url(&Config::fake_message(), &balloon);
        let expected = "<a href=\"https://example.com\"><div class=\"app_header\"><div class=\"name\">https://example.com</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_url_no_url_falls_back_to_msg_text() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let balloon = URLMessage {
            title: None,
            summary: None,
            url: None,
            original_url: None,
            item_type: None,
            images: vec![],
            icons: vec![],
            site_name: None,
            placeholder: false,
        };

        let mut msg = Config::fake_message();
        msg.text = Some("https://example.com/from-text".to_string());

        // No balloon URL → wrapper_url and name both fall back to msg.text.
        let actual = exporter.format_url(&msg, &balloon);
        let expected = "<a href=\"https://example.com/from-text\"><div class=\"app_header\"><div class=\"name\">https://example.com/from-text</div></div></a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_collaboration_no_url_with_original_url() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        // wrapper_url is gated on balloon.url; footer_url uses get_url() which
        // falls back to original_url. With url=None + original_url=Some, the
        // <a> wrapper is dropped but the footer subcaption still appears.
        let balloon = CollaborationMessage {
            original_url: Some("https://example.com/original"),
            url: None,
            title: Some("Doc title"),
            creation_date: None,
            bundle_id: Some("bundle"),
            app_name: Some("App"),
        };

        let actual = exporter.format_collaboration(&balloon);
        let expected = "<div class=\"app_header\"><div class=\"name\">App</div></div><div class=\"app_footer\"><div class=\"caption\">Doc title</div><div class=\"subcaption\">https://example.com/original</div></div>";

        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod text_effect_tests {
    use std::borrow::Cow;

    use imessage_database::{
        message_types::text_effects::{Animation, Style, TextEffect, Unit},
        tables::messages::models::{BubbleComponent, TextAttributes},
    };

    use crate::{
        Config, HTML, Options,
        app::export_type::ExportType::Html,
        exporters::formatter::{MessageFormatter, RenderContext, TextEffectFormatter},
    };

    #[test]
    fn can_format_html_default() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_effect("Chris", &TextEffect::Default);
        let expected = "Chris";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_mention() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_mention("Chris", "+15558675309");
        let expected = "<span title=\"+15558675309\"><b>Chris</b></span>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_link() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_link("chrissardegna.com", "https://chrissardegna.com");
        let expected = "<a href=\"https://chrissardegna.com\">chrissardegna.com</a>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_otp() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_otp("123456");
        let expected = "<u>123456</u>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_style_single() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_styles("Bold", &[Style::Bold]);
        let expected = "<b>Bold</b>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_style_multiple() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_styles("Bold", &[Style::Bold, Style::Strikethrough]);
        let expected = "<s><b>Bold</b></s>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_style_all() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_styles(
            "Bold",
            &[
                Style::Bold,
                Style::Strikethrough,
                Style::Italic,
                Style::Underline,
            ],
        );
        let expected = "<u><i><s><b>Bold</b></s></i></u>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_conversion() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_conversion("100 Miles", &Unit::Distance);
        let expected = "<u>100 Miles</u>";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_animated() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_animated("party", &Animation::Big);
        assert_eq!(actual, "<span class=\"animationBig\">party</span>");

        // Unknown(i64) round-trips its integer in the Debug form.
        let actual = exporter.format_animated("oops", &Animation::Unknown(42));
        assert_eq!(actual, "<span class=\"animationUnknown(42)\">oops</span>");
    }

    #[test]
    fn format_effect_default_is_borrowed() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let owned_text = String::from("hello");
        let result = exporter.format_effect(&owned_text, &TextEffect::Default);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Default arm must not allocate"
        );

        let owned_url = String::from("https://example.com");
        let link = TextEffect::Link(owned_url);
        let result = exporter.format_effect(&owned_text, &link);
        assert!(
            matches!(result, Cow::Owned(_)),
            "Link arm wraps in <a> and must own"
        );
    }

    #[test]
    fn format_mention_escapes_name_to_prevent_attribute_injection() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_mention("Chris", "\"><script>alert(1)</script>");
        assert_eq!(
            actual,
            "<span title=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\"><b>Chris</b></span>"
        );
        assert!(
            !actual.contains("<script>"),
            "raw <script> must not survive"
        );
    }

    #[test]
    fn format_link_escapes_url_to_prevent_attribute_injection() {
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let actual = exporter.format_link("click me", "https://x.test/?q=\"><script>");
        assert_eq!(
            actual,
            "<a href=\"https://x.test/?q=&quot;&gt;&lt;script&gt;\">click me</a>"
        );
        assert!(
            !actual.contains("<script>"),
            "raw <script> must not survive"
        );
    }

    #[test]
    fn can_format_html_mention_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Test Dad ".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 5, vec![TextEffect::Default]),
            TextAttributes::new(5, 8, vec![TextEffect::Mention("+15558675309".to_string())]),
            TextAttributes::new(8, 9, vec![TextEffect::Default]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Test <span title=\"+15558675309\"><b>Dad</b></span> </span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_otp_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("000123 is your security code. Don't share your code.".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 6, vec![TextEffect::OTP]),
            TextAttributes::new(6, 52, vec![TextEffect::Default]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><u>000123</u> is your security code. Don&apos;t share your code.</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_link_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("https://twitter.com/xxxxxxxxx/status/0000223300009216128".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![TextAttributes::new(
            0,
            56,
            vec![TextEffect::Link(
                "https://twitter.com/xxxxxxxxx/status/0000223300009216128".to_string(),
            )],
        )])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><a href=\"https://twitter.com/xxxxxxxxx/status/0000223300009216128\">https://twitter.com/xxxxxxxxx/status/0000223300009216128</a></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_conversion_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hi. Right now or tomorrow?".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 17, vec![TextEffect::Default]),
            TextAttributes::new(17, 25, vec![TextEffect::Conversion(Unit::Timezone)]),
            TextAttributes::new(25, 26, vec![TextEffect::Default]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">Hi. Right now or <u>tomorrow</u>?</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_effect_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Big small shake nod explode ripple bloom jitter".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 3, vec![TextEffect::Animated(Animation::Big)]),
            TextAttributes::new(3, 4, vec![TextEffect::Default]),
            TextAttributes::new(4, 10, vec![TextEffect::Animated(Animation::Small)]),
            TextAttributes::new(10, 15, vec![TextEffect::Animated(Animation::Shake)]),
            TextAttributes::new(15, 16, vec![TextEffect::Animated(Animation::Small)]),
            TextAttributes::new(16, 19, vec![TextEffect::Animated(Animation::Nod)]),
            TextAttributes::new(19, 20, vec![TextEffect::Animated(Animation::Small)]),
            TextAttributes::new(20, 28, vec![TextEffect::Animated(Animation::Explode)]),
            TextAttributes::new(28, 34, vec![TextEffect::Animated(Animation::Ripple)]),
            TextAttributes::new(34, 35, vec![TextEffect::Animated(Animation::Explode)]),
            TextAttributes::new(35, 40, vec![TextEffect::Animated(Animation::Bloom)]),
            TextAttributes::new(40, 41, vec![TextEffect::Animated(Animation::Explode)]),
            TextAttributes::new(41, 47, vec![TextEffect::Animated(Animation::Jitter)]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><span class=\"animationBig\">Big</span> <span class=\"animationSmall\">small </span><span class=\"animationShake\">shake</span><span class=\"animationSmall\"> </span><span class=\"animationNod\">nod</span><span class=\"animationSmall\"> </span><span class=\"animationExplode\">explode </span><span class=\"animationRipple\">ripple</span><span class=\"animationExplode\"> </span><span class=\"animationBloom\">bloom</span><span class=\"animationExplode\"> </span><span class=\"animationJitter\">jitter</span></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styles_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Bold underline italic strikethrough all four".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 4, vec![TextEffect::Styles(vec![Style::Bold])]),
            TextAttributes::new(4, 5, vec![TextEffect::Default]),
            TextAttributes::new(5, 14, vec![TextEffect::Styles(vec![Style::Underline])]),
            TextAttributes::new(14, 15, vec![TextEffect::Default]),
            TextAttributes::new(15, 21, vec![TextEffect::Styles(vec![Style::Italic])]),
            TextAttributes::new(21, 22, vec![TextEffect::Default]),
            TextAttributes::new(22, 35, vec![TextEffect::Styles(vec![Style::Strikethrough])]),
            TextAttributes::new(35, 40, vec![TextEffect::Default]),
            TextAttributes::new(
                40,
                44,
                vec![TextEffect::Styles(vec![
                    Style::Bold,
                    Style::Strikethrough,
                    Style::Underline,
                    Style::Italic,
                ])],
            ),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><b>Bold</b> <u>underline</u> <i>italic</i> <s>strikethrough</s> all <i><u><s><b>four</b></s></u></i></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styles_single_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Everything".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![TextAttributes::new(
            0,
            10,
            vec![TextEffect::Styles(vec![
                Style::Bold,
                Style::Strikethrough,
                Style::Underline,
                Style::Italic,
            ])],
        )])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><i><u><s><b>Everything</b></s></u></i></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styles_mixed_end_to_end() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Underline normal jitter normal".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 9, vec![TextEffect::Styles(vec![Style::Underline])]),
            TextAttributes::new(9, 17, vec![TextEffect::Default]),
            TextAttributes::new(17, 23, vec![TextEffect::Animated(Animation::Jitter)]),
            TextAttributes::new(23, 30, vec![TextEffect::Default]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><u>Underline</u> normal <span class=\"animationJitter\">jitter</span> normal</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styled_plain_link() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text =
            Some("https://github.com/ReagentX/imessage-exporter/discussions/553".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![TextAttributes::new(
            0,
            61,
            vec![
                TextEffect::Animated(Animation::Big),
                TextEffect::Link(
                    "https://github.com/ReagentX/imessage-exporter/discussions/553".to_string(),
                ),
            ],
        )])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><a href=\"https://github.com/ReagentX/imessage-exporter/discussions/553\"><span class=\"animationBig\">https://github.com/ReagentX/imessage-exporter/discussions/553</span></a></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styled_emoji_bold_underline() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("🅱️Bold_Underline".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(0, 7, vec![TextEffect::Default]),
            TextAttributes::new(7, 11, vec![TextEffect::Styles(vec![Style::Bold])]),
            TextAttributes::new(11, 12, vec![TextEffect::Default]),
            TextAttributes::new(12, 21, vec![TextEffect::Styles(vec![Style::Underline])]),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">🅱\u{fe0f}<b>Bold</b>_<u>Underline</u></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_text_styled_overlapping_ranges() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("8:00 pm".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![BubbleComponent::Text(vec![
            TextAttributes::new(
                0,
                1,
                vec![
                    TextEffect::Conversion(Unit::Timezone),
                    TextEffect::Styles(vec![Style::Bold]),
                ],
            ),
            TextAttributes::new(1, 2, vec![TextEffect::Conversion(Unit::Timezone)]),
            TextAttributes::new(
                2,
                4,
                vec![
                    TextEffect::Conversion(Unit::Timezone),
                    TextEffect::Styles(vec![Style::Underline]),
                ],
            ),
            TextAttributes::new(4, 5, vec![TextEffect::Conversion(Unit::Timezone)]),
            TextAttributes::new(
                5,
                7,
                vec![
                    TextEffect::Conversion(Unit::Timezone),
                    TextEffect::Styles(vec![Style::Italic]),
                ],
            ),
        ])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\"><b><u>8</u></b><u>:</u><u><u>00</u></u><u> </u><i><u>pm</u></i></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod edited_tests {
    use std::{env::current_dir, fs::File, io::Read};

    use crate::{
        Config, HTML, Options,
        app::export_type::ExportType::Html,
        exporters::formatter::{MessageFormatter, RenderContext},
    };
    use imessage_database::{
        message_types::{
            edited::{EditStatus, EditedEvent, EditedMessage, EditedMessagePart},
            text_effects::{Style, TextEffect},
        },
        tables::messages::models::{AttachmentMeta, BubbleComponent, TextAttributes},
    };

    #[test]
    fn can_format_html_edited_with_formatting() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        // Create edited message data
        let edited_message = EditedMessage {
            parts: vec![EditedMessagePart {
                status: EditStatus::Edited,
                edit_history: vec![
                    EditedEvent {
                        date: 758573156000000000,
                        text: Some("Test".to_string()),
                        components: vec![BubbleComponent::Text(vec![TextAttributes {
                            start: 0,
                            end: 4,
                            effects: vec![TextEffect::Default],
                        }])],
                        guid: None,
                    },
                    EditedEvent {
                        date: 758573166000000000,
                        text: Some("Test".to_string()),
                        components: vec![BubbleComponent::Text(vec![TextAttributes {
                            start: 0,
                            end: 4,
                            effects: vec![TextEffect::Styles(vec![Style::Strikethrough])],
                        }])],
                        guid: Some("76A466B8-D21E-4A20-AF62-FF2D3A20D31C".to_string()),
                    },
                ],
            }],
        };

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.date_edited = 674530231992568192;
        message.text = Some("Test".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.edited_parts = Some(edited_message);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/EditedWithFormatting");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        message.components = vec![BubbleComponent::Text(vec![TextAttributes::new(
            0,
            4,
            vec![TextEffect::Styles(vec![Style::Strikethrough])],
        )])];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <div class=\"edited\"><table><tbody>\n        <tr>\n            <td><span class=\"timestamp\"></span></td>\n            <td>Test</td>\n        </tr>\n    </tbody><tfoot>\n        <tr>\n            <td><span class=\"timestamp\">Edited 10 seconds later</span></td>\n            <td><s>Test</s></td>\n        </tr>\n    </tfoot></table></div>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_conversion_final_unsent() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.date_edited = 674530231992568192;
        message.text = Some(
            "From arbitrary byte stream:\r\u{FFFC}To native Rust data structures:\r".to_string(),
        );
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.edited_parts = Some(EditedMessage {
            parts: vec![
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Unsent,
                    edit_history: vec![],
                },
            ],
        });

        message.components = vec![
            BubbleComponent::Text(vec![TextAttributes::new(0, 28, vec![TextEffect::Default])]),
            BubbleComponent::Attachment(AttachmentMeta {
                guid: Some("D0551D89-4E11-43D0-9A0E-06F19704E97B".to_string()),
                transcription: None,
                height: None,
                width: None,
                name: None,
            }),
            BubbleComponent::Text(vec![TextAttributes::new(31, 63, vec![TextEffect::Default])]),
            BubbleComponent::Retracted,
        ];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">From arbitrary byte stream:\r</span>\n    </div>\n\n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"attachment_error\">Attachment does not exist!</span>\n    </div>\n\n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">To native Rust data structures:\r</span>\n    </div>\n\n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"unsent\"><span class=\"unsent\">You unsent this message part 1 hour, 49 seconds after sending!</span></span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_conversion_no_edits() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some(
            "From arbitrary byte stream:\r\u{FFFC}To native Rust data structures:\r".to_string(),
        );
        message.is_from_me = true;
        message.chat_id = Some(0);

        message.components = vec![
            BubbleComponent::Text(vec![TextAttributes::new(0, 28, vec![TextEffect::Default])]),
            BubbleComponent::Attachment(AttachmentMeta {
                guid: Some("D0551D89-4E11-43D0-9A0E-06F19704E97B".to_string()),
                transcription: None,
                height: None,
                width: None,
                name: None,
            }),
            BubbleComponent::Text(vec![TextAttributes::new(31, 63, vec![TextEffect::Default])]),
        ];

        let mut actual = String::new();
        exporter
            .format_message_into(&message, RenderContext::TopLevel, &mut actual)
            .unwrap();
        let expected = "<div class=\"message\">\n    <div class=\"sent iMessage\">\n        <p>\n            <span class=\"timestamp\">\n                <a title=\"Reveal in Messages app\" href=\"sms://open?message-guid=\">May 17, 2022  5:29:42 PM</a>\n                \n            </span>\n            \n            <span class=\"sender\">Me</span>\n        </p>\n        \n        \n        \n        \n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">From arbitrary byte stream:\r</span>\n    </div>\n\n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"attachment_error\">Attachment does not exist!</span>\n    </div>\n\n        \n        <hr>\n<div class=\"message_part\">\n    <span class=\"bubble\">To native Rust data structures:\r</span>\n    </div>\n\n        \n        \n    </div>\n</div>\n";

        assert_eq!(actual, expected);
    }

    #[test]
    fn can_format_html_conversion_fully_unsent() {
        // Create exporter
        let options = Options::fake_options(Html);
        let config = Config::fake_app(options);
        let exporter = HTML::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.date_edited = 674530231992568192;
        message.text = None;
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.edited_parts = Some(EditedMessage {
            parts: vec![EditedMessagePart {
                status: EditStatus::Unsent,
                edit_history: vec![],
            }],
        });

        message.components = vec![];

        let mut actual = String::new();
        exporter.format_announcement(&message, &mut actual);
        let expected = "<div class=\"announcement\">\n    <p><span class=\"timestamp\">May 17, 2022  5:29:42 PM</span> You unsent a message.</p>\n</div>";

        assert_eq!(actual, expected);
    }
}

// MARK: Forensic integration tests
//
// These tests run the full `run_export` pipeline against a checked-in
// fixture database (`forensic_fixture.db`) containing deliberate scenarios:
// added/removed tapbacks, custom-emoji tapback, orphan tapback, reply,
// edited message. They assert on the actual rendered HTML so changes that
// break end-to-end behavior (the snippet-bug class) get caught in CI.
#[cfg(test)]
mod forensic_integration_tests {
    use std::{env::current_dir, path::PathBuf};

    use crate::{
        Config, HTML, Options,
        app::export_type::ExportType,
        exporters::shared::driver::run_export,
    };

    fn fixture_db_path() -> PathBuf {
        current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/db/forensic_fixture.db")
    }

    fn export_dir(test_name: &str) -> PathBuf {
        // Per-test directory so parallel-running tests can't clash.
        std::env::temp_dir().join(format!("forensic_integration_{test_name}"))
    }

    fn export_to_string(test_name: &str, forensic: bool) -> String {
        let mut options = Options::fake_options(ExportType::Html);
        options.db_path = fixture_db_path();
        options.forensic = forensic;
        let dir = export_dir(test_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        options.export_path = dir.clone();

        let config = Config::fake_app(options);
        let mut writer = HTML::new(&config).unwrap();
        run_export(&mut writer).unwrap();

        let html = std::fs::read_to_string(dir.join("orphaned.html"))
            .expect("orphaned.html should exist after run_export");
        let _ = std::fs::remove_dir_all(&dir);
        html
    }

    #[test]
    fn forensic_full_pipeline_renders_meta_strip_on_messages() {
        let html = export_to_string("meta_strip", true);
        assert!(
            html.contains("class=\"forensic_meta\""),
            "expected forensic_meta strip in output",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_reply_with_parent_text_snippet() {
        // Regression: if resolve_replying_to skips apply_body, this assertion
        // fails because the snippet falls back to "[attachment]".
        let html = export_to_string("reply_snippet", true);
        assert!(
            html.contains("class=\"replying_to\""),
            "expected replying_to quote header",
        );
        assert!(
            html.contains("eat as quick as possible"),
            "expected parent's real text in the reply snippet, got placeholder?",
        );
        assert!(
            html.contains("href=\"#0355C6E1-D0C8-4212-AA87-DD8AE4FD1203\""),
            "expected anchor link to parent guid",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_added_tapback_bubble_with_in_reaction_to() {
        let html = export_to_string("added_tapback_bubble", true);
        assert!(
            html.contains("id=\"F0R3N51C-0002-LOVE-ADDD-EDFFFFFFFFFF\""),
            "expected added-tapback bubble anchor",
        );
        // Same snippet regression check applies to in_reaction_to.
        assert!(
            html.contains("eat as quick as possible"),
            "expected target's real text in in_reaction_to snippet",
        );
        assert!(
            html.contains("Added by"),
            "expected Added phrasing in tapback summary",
        );
        assert!(
            html.contains(">Loved<"),
            "expected Loved kind in tapback summary",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_removed_tapback_bubble_with_removed_class() {
        let html = export_to_string("removed_tapback_bubble", true);
        assert!(
            html.contains("tapback_bubble_removed"),
            "expected tapback_bubble_removed class for removed tapback",
        );
        assert!(
            html.contains("id=\"F0R3N51C-0003-LIKE-REMD-EDFFFFFFFFFF\""),
            "expected removed-tapback bubble anchor",
        );
        assert!(
            html.contains("Removed by"),
            "expected Removed phrasing in tapback summary",
        );
        assert!(
            html.contains(">Liked<"),
            "expected Liked kind for removed tapback",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_custom_emoji_tapback() {
        let html = export_to_string("custom_emoji_tapback", true);
        assert!(
            html.contains("id=\"F0R3N51C-0004-EMJI-CFFE-EDFFFFFFFFFF\""),
            "expected custom-emoji-tapback bubble anchor",
        );
        // Coffee + variation selector: U+2615 U+FE0F.
        assert!(
            html.contains("\u{2615}\u{fe0f}"),
            "expected coffee emoji in custom-emoji tapback bubble",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_orphan_tapback_without_in_reaction_to() {
        let html = export_to_string("orphan_tapback", true);
        assert!(
            html.contains("id=\"F0R3N51C-0005-LOVE-ORPH-EDFFFFFFFFFF\""),
            "expected orphan-tapback bubble to render even when target is missing",
        );
        // Scope the in_reaction_to check to the orphan's bubble only — other
        // tapbacks in the same export legitimately have headers.
        let needle = "id=\"F0R3N51C-0005-LOVE-ORPH-EDFFFFFFFFFF\"";
        let start = html.find(needle).expect("orphan bubble anchor present");
        let tail = &html[start..];
        let bubble_end = tail.find("</div></div>").unwrap_or(tail.len());
        let bubble = &tail[..bubble_end];
        assert!(
            !bubble.contains("in_reaction_to"),
            "orphan tapback bubble must omit in_reaction_to header, got: {bubble}",
        );
    }

    #[test]
    fn forensic_full_pipeline_renders_edited_flag_on_edited_message() {
        let html = export_to_string("edited_flag", true);
        let needle = "id=\"F0R3N51C-0006-EDIT-EDIT-EDFFFFFFFFFF\"";
        let start = html.find(needle).expect("edited message anchor not found");
        let tail = &html[start..];
        let bubble_end = tail.find("</div>\n</div>").unwrap_or(tail.len());
        let bubble = &tail[..bubble_end];
        assert!(
            bubble.contains(">edited<"),
            "expected `edited` flag inside the forensic_meta strip of the edited message, got: {bubble}",
        );
    }
}
