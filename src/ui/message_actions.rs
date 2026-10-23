//! The label and icon of each action on a message, as the reader's ⋯ menu
//! in the main window and the pop-out's both show them. One table, so the
//! two menus cannot drift apart. (The list's row menu and the card menu
//! word their entries differently and keep their own.)

use crate::i18n::i18n;

/// One entry of a message's ⋯ menu. The two-faced actions (star, spam,
/// read) have one variant per face; the caller picks by the message's
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageAction {
    Reply,
    ReplyAll,
    Forward,
    Star,
    RemoveStar,
    Archive,
    Delete,
    MarkSpam,
    NotSpam,
    MarkRead,
    MarkUnread,
    Tags,
    MoveToInbox,
    MoveTo,
    Find,
    PrintPreview,
}

impl MessageAction {
    /// The entry's text, translated.
    pub(crate) fn label(self) -> String {
        match self {
            Self::Reply => i18n("Reply"),
            Self::ReplyAll => i18n("Reply All"),
            Self::Forward => i18n("Forward"),
            Self::Star => i18n("Star"),
            Self::RemoveStar => i18n("Remove Star"),
            Self::Archive => i18n("Archive"),
            Self::Delete => i18n("Delete"),
            Self::MarkSpam => i18n("Mark as Spam"),
            Self::NotSpam => i18n("Not Spam"),
            Self::MarkRead => i18n("Mark as Read"),
            Self::MarkUnread => i18n("Mark as Unread"),
            Self::Tags => i18n("Tags"),
            Self::MoveToInbox => i18n("Move to Inbox"),
            Self::MoveTo => i18n("Move To…"),
            Self::Find => i18n("Find in Message"),
            Self::PrintPreview => i18n("Print Preview"),
        }
    }

    /// The entry's icon, without the `-symbolic` the menus add.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Reply => "mail-reply-sender",
            Self::ReplyAll => "mail-reply-all",
            Self::Forward => "mail-forward",
            Self::Star => "starred",
            Self::RemoveStar => "hylki-non-starred",
            Self::Archive => "mail-archive",
            Self::Delete => "user-trash",
            Self::MarkSpam => "mail-mark-junk",
            Self::NotSpam => "mail-mark-notjunk",
            Self::MarkRead => "hylki-mail-read",
            Self::MarkUnread => "mail-unread",
            Self::Tags => "tag-outline",
            Self::MoveToInbox => "mail-inbox",
            Self::MoveTo => "folder",
            Self::Find => "loupe-with-arrow",
            Self::PrintPreview => "printer",
        }
    }
}
