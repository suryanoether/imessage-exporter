use imessage_database::{
    error::table::TableError,
    message_types::variants::{Tapback, TapbackAction, Variant},
    tables::{attachment::Attachment, messages::Message},
};

use crate::{app::runtime::Config, exporters::shared::time::format_message_date};

/// Format-agnostic shape for tapback rendering. The `payload` carried by
/// [`Sticker`] is generic so each exporter can use its own pre-rendered type.
///
/// [`Sticker`]: TapbackKind::Sticker
pub enum TapbackKind<'a, S> {
    /// Standard reaction.
    Reaction { tapback: Tapback<'a>, who: &'a str },
    /// Sticker tapback whose attachment was found and rendered.
    Sticker { payload: S, who: &'a str },
    /// Sticker tapback whose attachment is missing.
    StickerMissing { who: &'a str },
}

/// Forensic metadata for a tapback. Populated only when `--forensic` is on.
/// Carried alongside the [`TapbackKind`] so templates can render an action
/// label and timestamp without re-deriving the variant.
pub(crate) struct TapbackForensic {
    pub action: TapbackAction,
    pub timestamp: String,
}

/// The output of [`resolve_tapback`]: the format-agnostic [`TapbackKind`]
/// together with optional forensic metadata.
pub(crate) struct ResolvedTapback<'a, S> {
    pub kind: TapbackKind<'a, S>,
    pub forensic: Option<TapbackForensic>,
}

/// Resolve a tapback message into the format-agnostic [`TapbackKind`].
///
/// Without `--forensic`, [`TapbackAction::Removed`] rows return `Ok(None)`
/// so the caller renders an empty string. With `--forensic`, every tapback
/// row (Added and Removed) returns `Ok(Some(_))` carrying the action and
/// the row's timestamp.
///
/// `sticker_renderer` lifts a found sticker attachment into the format's
/// payload type. The closure captures the formatter `self` and `msg` so its
/// body can call back into [`format_sticker`]. For Removed-action sticker
/// rows in forensic mode, the sticker payload is *not* rendered; the
/// removal is surfaced as a `Reaction { tapback: Tapback::Sticker, .. }`
/// so the template can produce text like "Sticker removed by Alice".
///
/// [`format_sticker`]: crate::exporters::formatter::MessageFormatter::format_sticker
///
/// # Panics
///
/// Panics if `msg.variant()` is not [`Variant::Tapback`]. Calling code is
/// expected to dispatch off the variant before calling this helper.
pub(crate) fn resolve_tapback<'a, S>(
    msg: &'a Message,
    config: &'a Config,
    sticker_renderer: impl FnOnce(&mut Attachment) -> S,
) -> Result<Option<ResolvedTapback<'a, S>>, TableError> {
    let Variant::Tapback(_, action, tapback) = msg.variant() else {
        unreachable!(
            "resolve_tapback called with non-Tapback variant: {:?}",
            msg.variant()
        )
    };

    let forensic_enabled = config.options.forensic;
    let is_removed = matches!(action, TapbackAction::Removed);

    if is_removed && !forensic_enabled {
        return Ok(None);
    }

    let who = config.who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id);

    let kind = if is_removed {
        // In forensic mode we do not render the sticker payload for removed
        // rows. Collapse every removed tapback to its `Reaction` form so the
        // template renders "<kind> removed by <who>" uniformly.
        TapbackKind::Reaction { tapback, who }
    } else {
        match tapback {
            Tapback::Sticker => {
                let mut paths = Attachment::from_message(config.data_source.db(), msg)?;
                match paths.get_mut(0) {
                    Some(sticker) => TapbackKind::Sticker {
                        payload: sticker_renderer(sticker),
                        who,
                    },
                    None => TapbackKind::StickerMissing { who },
                }
            }
            other => TapbackKind::Reaction {
                tapback: other,
                who,
            },
        }
    };

    let forensic = forensic_enabled.then(|| TapbackForensic {
        action,
        timestamp: format_message_date(msg, config.offset),
    });

    Ok(Some(ResolvedTapback { kind, forensic }))
}
