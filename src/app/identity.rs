//! Accounts as the interface shows them: names, colors, faces and the
//! addresses they send from.

use relm4::prelude::*;

use crate::config::{self, AccountConfig};
use crate::ui::message_list::MessageListInput;
use crate::ui::message_view::MessageViewInput;
use crate::ui::message_window::MessageWindowInput;

use super::demo::demo_mode;
use super::model::AppModel;
use super::msg::AppMsg;

impl AppModel {
    /// The account to act on by default: the selected folder's account,
    /// else the first in sidebar order. With All Inboxes open that is the
    /// account a new message starts from, so it has to be the one the
    /// person put on top, not the one added first (#261).
    pub(super) fn active_account(&self) -> u32 {
        self.selected
            .as_ref()
            .map(|s| s.account_id)
            .or_else(|| {
                let first = self.ordered_emails().into_iter().next()?;
                self.accounts
                    .iter()
                    .find(|a| a.email == first)
                    .map(|a| a.id)
            })
            .or_else(|| self.accounts.first().map(|a| a.id))
            .unwrap_or(1)
    }

    /// Resolved avatar/accent color for an account (custom, else auto accent).
    pub(super) fn account_color(&self, account_id: u32) -> String {
        self.account_config(account_id)
            .and_then(|c| c.color.clone())
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.id == account_id)
                    .map(|a| a.accent.clone())
            })
            .unwrap_or_else(|| "#3584e4".to_string())
    }

    /// Every address the accounts send from, their aliases included, lower
    /// case.
    pub(super) fn own_addresses(&self) -> std::collections::HashSet<String> {
        self.effective_config()
            .iter()
            .flat_map(|a| {
                std::iter::once(a.email.clone()).chain(
                    a.aliases
                        .iter()
                        .map(|alias| config::split_identity(&alias.identity).1),
                )
            })
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    }

    /// The configs the UI describes accounts from: the real ones, or the
    /// demo's in-memory stand-ins while no account is configured.
    pub(super) fn effective_config(&self) -> &[AccountConfig] {
        if self.config.is_empty() && demo_mode() {
            &self.demo_config
        } else {
            &self.config
        }
    }

    /// An account's settings by id (an account's id is its place in the
    /// config plus one), the demo's stand-ins included. Id 0 reads the
    /// first account.
    pub(super) fn account_config(&self, account_id: u32) -> Option<&AccountConfig> {
        self.effective_config()
            .get(account_id.saturating_sub(1) as usize)
    }

    /// An account's saved settings by id, never the demo's stand-ins. Id 0
    /// reads the first account.
    pub(super) fn saved_account_config(&self, account_id: u32) -> Option<&AccountConfig> {
        self.config.get(account_id.saturating_sub(1) as usize)
    }

    /// An account's avatar picture (#162), when one is set and its file is
    /// still there.
    pub(super) fn account_avatar(&self, account_id: u32) -> Option<std::path::PathBuf> {
        self.account_config(account_id)
            .and_then(|c| c.avatar.as_deref())
            .and_then(config::avatar_path)
    }

    /// Custom avatar emoji for an account, if set.
    pub(super) fn account_emoji(&self, account_id: u32) -> Option<String> {
        // In demo mode the sample accounts' emoji come from the stand-in
        // configs, which the Accounts panel can edit.
        self.account_config(account_id)
            .and_then(|c| c.emoji.clone())
    }

    /// Tell the face chain which addresses are the user's own, and what their
    /// accounts show for them (#189): a mailbox given a picture wears it on
    /// the mail it sent too — in a conversation's cards and in the list — and
    /// not only in the sidebar. Refreshed with the sidebar, which is rebuilt
    /// whenever the accounts change.
    pub(super) fn refresh_own_faces(&self) {
        crate::services::avatar::set_faces_on_own_mail(self.own_mailbox_face);
        let mut faces: Vec<(String, crate::services::avatar::OwnFace)> = Vec::new();
        for (i, cfg) in self.effective_config().iter().enumerate() {
            let face = crate::services::avatar::OwnFace {
                gravatar: cfg.gravatar,
                picture: cfg.avatar.as_deref().and_then(config::avatar_path),
                emoji: cfg.emoji.clone().filter(|e| !e.trim().is_empty()),
                color: self.account_color(i as u32 + 1),
            };
            // An account showing its initials has nothing to add: the circle
            // its own messages already get is drawn from the same letters.
            if !face.is_set() {
                continue;
            }
            // Mail sent as a send-as alias (#34) is still from this mailbox.
            let addresses = std::iter::once(cfg.email.clone()).chain(
                cfg.aliases
                    .iter()
                    .map(|al| config::split_identity(&al.identity).1),
            );
            faces.extend(
                addresses
                    .filter(|address| !address.trim().is_empty())
                    .map(|address| (address, face.clone())),
            );
        }
        crate::services::avatar::set_own_faces(faces);
    }

    /// Redraw the faces on screen after what a mailbox shows has changed
    /// (#189): the list rebuilds its rows, and the reader — in the pane and
    /// in any popped-out window — its cards.
    pub(super) fn refresh_faces(&self) {
        self.message_list
            .emit(MessageListInput::ContactPhotosChanged);
        self.message_view.emit(MessageViewInput::FacesChanged);
        self.popouts_emit(|| MessageWindowInput::FacesChanged);
    }

    /// Look up the Gravatars the accounts asked for (#189), one request per
    /// address per session, off the main thread. Done here rather than where
    /// a circle is drawn so every view — the sidebar, the list, an open
    /// conversation — is holding the same answer.
    pub(super) fn warm_own_gravatars(&self, sender: &ComponentSender<Self>) {
        for cfg in self.effective_config() {
            if !cfg.gravatar {
                continue;
            }
            // An alias is an address of its own, and may have a Gravatar of
            // its own; each is asked about separately.
            let addresses = std::iter::once(cfg.email.clone()).chain(
                cfg.aliases
                    .iter()
                    .map(|al| config::split_identity(&al.identity).1),
            );
            for address in addresses.filter(|a| !a.trim().is_empty()) {
                if !crate::services::avatar::wants_own_gravatar(&address) {
                    continue;
                }
                let back = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let outcome = crate::services::avatar::fetch_own_gravatar(&address);
                    let _ = back.send(AppMsg::OwnGravatarFetched {
                        email: address,
                        outcome,
                    });
                });
            }
        }
    }

    /// A label for an account in messages (name, else email, else "Account N").
    /// Uses config (available even before the account connects), then live data.
    pub(super) fn account_label(&self, account_id: u32) -> String {
        let pick = |name: &str, email: &str| -> Option<String> {
            if !name.trim().is_empty() {
                Some(name.to_string())
            } else if !email.trim().is_empty() {
                Some(email.to_string())
            } else {
                None
            }
        };
        self.saved_account_config(account_id)
            .and_then(|c| pick(&c.name, &c.email))
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.id == account_id)
                    .and_then(|a| pick(&a.name, &a.email))
            })
            .unwrap_or_else(|| format!("Account {account_id}"))
    }

    /// Display name for an account (name, else email).
    pub(super) fn account_name(&self, account_id: u32) -> String {
        // The account's UI label (how it's shown in All Inboxes / the reader chip).
        self.accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.label.clone())
            .unwrap_or_default()
    }

    /// The account email for an id, if known.
    pub(super) fn email_of(&self, account_id: u32) -> Option<String> {
        self.accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.email.clone())
    }

    /// Every address this account sends as: its own, plus its send-as
    /// aliases (#34). Lower-cased comparison is the caller's job.
    pub(super) fn own_identities(&self, account_id: u32) -> Vec<String> {
        let Some(cfg) = self.account_config(account_id) else {
            return self.email_of(account_id).into_iter().collect();
        };
        std::iter::once(cfg.email.clone())
            .chain(
                cfg.aliases
                    .iter()
                    .map(|al| config::split_identity(&al.identity).1),
            )
            .filter(|a| !a.trim().is_empty())
            .collect()
    }

    /// The messages the router sends here (see `AppModel::route`).
    pub(super) fn on_identity(&self, msg: AppMsg) {
        match msg {
            AppMsg::OwnGravatarFetched { email, outcome } => {
                // Nothing to redraw unless the address turned out to have one.
                if crate::services::avatar::cache_own_gravatar(&email, outcome) {
                    self.rebuild_sidebar();
                    self.refresh_faces();
                }
            }
            _ => unreachable!("not routed to on_identity"),
        }
    }
}
