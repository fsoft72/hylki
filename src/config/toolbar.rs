//! The reader toolbar's layout: which buttons it shows, on which side,
//! and toolbar.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{config_file, read_toml, save_plain_toml};

// ---------------------------------------------------------------------------
// Reader toolbar layout: which buttons the reading pane's header shows, on
// which side, in what order (Settings → Appearance → Toolbar).
// ---------------------------------------------------------------------------

/// One button of the reader header. The left group stays on the bar at every
/// width; the right group folds into the ⋯ overflow menu when the pane is
/// narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ToolbarItem {
    Reply,
    ReplyAll,
    Forward,
    Star,
    Archive,
    Delete,
    Spam,
    ReadUnread,
    Tags,
    MoveTo,
    Find,
    Print,
}

impl ToolbarItem {
    pub(crate) const ALL: [ToolbarItem; 12] = [
        ToolbarItem::Reply,
        ToolbarItem::ReplyAll,
        ToolbarItem::Forward,
        ToolbarItem::Star,
        ToolbarItem::Archive,
        ToolbarItem::Delete,
        ToolbarItem::Spam,
        ToolbarItem::ReadUnread,
        ToolbarItem::Tags,
        ToolbarItem::MoveTo,
        ToolbarItem::Find,
        ToolbarItem::Print,
    ];

    /// The stable name written to `toolbar.toml`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            ToolbarItem::Reply => "reply",
            ToolbarItem::ReplyAll => "reply_all",
            ToolbarItem::Forward => "forward",
            ToolbarItem::Star => "star",
            ToolbarItem::Archive => "archive",
            ToolbarItem::Delete => "delete",
            ToolbarItem::Spam => "spam",
            ToolbarItem::ReadUnread => "read_unread",
            ToolbarItem::Tags => "tags",
            ToolbarItem::MoveTo => "move_to",
            ToolbarItem::Find => "find",
            ToolbarItem::Print => "print",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<ToolbarItem> {
        ToolbarItem::ALL.iter().copied().find(|i| i.key() == key)
    }

    /// The symbolic icon the button (and the settings chip) wears.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            ToolbarItem::Reply => "mail-reply-sender-symbolic",
            ToolbarItem::ReplyAll => "mail-reply-all-symbolic",
            ToolbarItem::Forward => "mail-forward-symbolic",
            ToolbarItem::Star => "hylki-non-starred-symbolic",
            ToolbarItem::Archive => "mail-archive-symbolic",
            ToolbarItem::Delete => "user-trash-symbolic",
            ToolbarItem::Spam => "mail-mark-junk-symbolic",
            ToolbarItem::ReadUnread => "mail-unread-symbolic",
            ToolbarItem::Tags => "tag-outline-symbolic",
            ToolbarItem::MoveTo => "folder-symbolic",
            ToolbarItem::Find => "loupe-with-arrow-symbolic",
            ToolbarItem::Print => "printer-symbolic",
        }
    }

    /// The untranslated label (callers pass it through `i18n`).
    pub(crate) fn label(self) -> &'static str {
        match self {
            ToolbarItem::Reply => crate::i18n::i18n_noop("Reply"),
            ToolbarItem::ReplyAll => crate::i18n::i18n_noop("Reply All"),
            ToolbarItem::Forward => crate::i18n::i18n_noop("Forward"),
            ToolbarItem::Star => crate::i18n::i18n_noop("Star"),
            ToolbarItem::Archive => crate::i18n::i18n_noop("Archive"),
            ToolbarItem::Delete => crate::i18n::i18n_noop("Delete"),
            ToolbarItem::Spam => crate::i18n::i18n_noop("Spam"),
            ToolbarItem::ReadUnread => crate::i18n::i18n_noop("Read/Unread"),
            ToolbarItem::Tags => crate::i18n::i18n_noop("Tags"),
            ToolbarItem::MoveTo => crate::i18n::i18n_noop("Move To"),
            ToolbarItem::Find => crate::i18n::i18n_noop("Find"),
            ToolbarItem::Print => crate::i18n::i18n_noop("Print"),
        }
    }
}

/// Which side of the reader header a button sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolbarSide {
    Left,
    Right,
}

/// How many buttons one side of the reader toolbar holds at most.
pub(crate) const TOOLBAR_SIDE_MAX: usize = 6;

/// The reader header's layout. A button absent from both lists is hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReaderToolbar {
    pub(crate) left: Vec<ToolbarItem>,
    pub(crate) right: Vec<ToolbarItem>,
}

impl Default for ReaderToolbar {
    fn default() -> Self {
        ReaderToolbar {
            left: vec![
                ToolbarItem::Reply,
                ToolbarItem::ReplyAll,
                ToolbarItem::Forward,
                ToolbarItem::Star,
                ToolbarItem::Archive,
                ToolbarItem::Delete,
            ],
            right: vec![
                ToolbarItem::Tags,
                ToolbarItem::ReadUnread,
                ToolbarItem::Spam,
                ToolbarItem::MoveTo,
                ToolbarItem::Find,
                ToolbarItem::Print,
            ],
        }
    }
}

impl ReaderToolbar {
    /// The side an item sits on, or None when it is hidden.
    pub(crate) fn side(&self, item: ToolbarItem) -> Option<ToolbarSide> {
        if self.left.contains(&item) {
            Some(ToolbarSide::Left)
        } else if self.right.contains(&item) {
            Some(ToolbarSide::Right)
        } else {
            None
        }
    }

    /// The items on neither side, in canonical order.
    pub(crate) fn hidden(&self) -> Vec<ToolbarItem> {
        ToolbarItem::ALL
            .iter()
            .copied()
            .filter(|i| self.side(*i).is_none())
            .collect()
    }

    /// Drop repeats (an item may sit on one side only, once), and anything
    /// past a side's room.
    fn sanitize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.left.retain(|i| seen.insert(*i));
        self.right.retain(|i| seen.insert(*i));
        self.left.truncate(TOOLBAR_SIDE_MAX);
        self.right.truncate(TOOLBAR_SIDE_MAX);
    }

    /// Whether `side` has room for one more button that is not already on
    /// it (`None`, the hidden zone, always has).
    pub(crate) fn has_room(&self, side: Option<ToolbarSide>, item: ToolbarItem) -> bool {
        let list = match side {
            Some(ToolbarSide::Left) => &self.left,
            Some(ToolbarSide::Right) => &self.right,
            None => return true,
        };
        list.contains(&item) || list.len() < TOOLBAR_SIDE_MAX
    }

    /// Take `item` out of wherever it is and put it at `index` of `side`
    /// (clamped); `side` None hides it. A full side is left as it is.
    pub(crate) fn place(&mut self, item: ToolbarItem, side: Option<ToolbarSide>, index: usize) {
        if !self.has_room(side, item) {
            return;
        }
        self.left.retain(|i| *i != item);
        self.right.retain(|i| *i != item);
        let list = match side {
            Some(ToolbarSide::Left) => &mut self.left,
            Some(ToolbarSide::Right) => &mut self.right,
            None => return,
        };
        let index = index.min(list.len());
        list.insert(index, item);
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct ToolbarFile {
    #[serde(default)]
    pub(super) left: Vec<String>,
    #[serde(default)]
    pub(super) right: Vec<String>,
}

fn toolbar_path() -> Option<PathBuf> {
    config_file("toolbar.toml")
}

/// The saved reader toolbar layout, or the default when there is none.
/// Unknown names (a newer Hylki's buttons) are skipped, not fatal.
pub(crate) fn load_reader_toolbar() -> ReaderToolbar {
    let Some(file) = read_toml::<ToolbarFile>(toolbar_path()) else {
        return ReaderToolbar::default();
    };
    let mut layout = ReaderToolbar {
        left: file
            .left
            .iter()
            .filter_map(|k| ToolbarItem::from_key(k))
            .collect(),
        right: file
            .right
            .iter()
            .filter_map(|k| ToolbarItem::from_key(k))
            .collect(),
    };
    layout.sanitize();
    layout
}

pub(crate) fn save_reader_toolbar(layout: &ReaderToolbar) {
    let file = ToolbarFile {
        left: layout.left.iter().map(|i| i.key().to_string()).collect(),
        right: layout.right.iter().map(|i| i.key().to_string()).collect(),
    };
    save_plain_toml(toolbar_path(), &file);
}

#[cfg(test)]
mod toolbar_tests {
    use super::*;

    #[test]
    fn place_moves_between_and_within_sides() {
        let mut t = ReaderToolbar::default();
        // The default left group is full: a move onto it changes nothing.
        t.place(ToolbarItem::Print, Some(ToolbarSide::Left), 0);
        assert_eq!(t.left[0], ToolbarItem::Reply);
        assert_eq!(t.right.last(), Some(&ToolbarItem::Print));
        // Make room, then right → left, at the front.
        t.place(ToolbarItem::Star, None, 0);
        t.place(ToolbarItem::Print, Some(ToolbarSide::Left), 0);
        assert_eq!(t.left[0], ToolbarItem::Print);
        assert!(!t.right.contains(&ToolbarItem::Print));
        // Within the left group: Reply (now index 1) to the end.
        t.place(ToolbarItem::Reply, Some(ToolbarSide::Left), 99);
        assert_eq!(t.left.last(), Some(&ToolbarItem::Reply));
        assert_eq!(t.left.len(), TOOLBAR_SIDE_MAX);
        // Hidden: on neither side, listed under hidden().
        t.place(ToolbarItem::Spam, None, 0);
        assert_eq!(t.side(ToolbarItem::Spam), None);
        assert_eq!(t.hidden(), vec![ToolbarItem::Star, ToolbarItem::Spam]);
        // Back from hidden into the right group's middle.
        t.place(ToolbarItem::Spam, Some(ToolbarSide::Right), 1);
        assert_eq!(t.right[1], ToolbarItem::Spam);
        assert_eq!(t.hidden(), vec![ToolbarItem::Star]);
    }

    #[test]
    fn file_round_trip_skips_unknown_names() {
        let file: ToolbarFile =
            toml::from_str("left = [\"reply\", \"bogus\", \"delete\"]\nright = [\"print\"]\n")
                .unwrap();
        let mut layout = ReaderToolbar {
            left: file
                .left
                .iter()
                .filter_map(|k| ToolbarItem::from_key(k))
                .collect(),
            right: file
                .right
                .iter()
                .filter_map(|k| ToolbarItem::from_key(k))
                .collect(),
        };
        layout.sanitize();
        assert_eq!(layout.left, vec![ToolbarItem::Reply, ToolbarItem::Delete]);
        assert_eq!(layout.right, vec![ToolbarItem::Print]);
        for item in ToolbarItem::ALL {
            assert_eq!(ToolbarItem::from_key(item.key()), Some(item));
        }
    }
}
