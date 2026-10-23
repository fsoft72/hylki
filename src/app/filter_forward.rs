//! Forwarding mail from a filter: the same message the manual Forward would
//! write, built without a composer once the original's body and attachments
//! are in hand, and recorded in the [`config::ForwardedLog`] once it is on its
//! way. The rest of a rule's actions wait for that record (see
//! `AppModel::filter_pass`).

use crate::i18n::i18n_f;
use crate::mail::quote::quoted_original;
use crate::models::Message;
use crate::worker::{MailRequest, OutgoingMessage};

use super::conversation::{body_key, is_event_target, BodyKey};
use super::filter_actions::ForwardJob;
use super::model::AppModel;
use super::prefill::{forward_subject, stage_attachments};

/// How long a forward may wait for its message's body and files before the
/// filters give up on that attempt and let the next pass start it again.
const AUTO_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// A forward handed to a worker, until the worker says how it went.
pub(super) struct SentForward {
    key: String,
    rule: String,
    /// What the worker hands back if the server refuses it (`Unsent`): the
    /// original's Message-ID and the destinations, to tell it from a
    /// message of the user's.
    forwards: String,
    to: String,
    /// The server refused it: it is not queued anywhere, so it was not
    /// forwarded and must be tried again.
    refused: bool,
}

/// A forward waiting for the body or the attachments of its message.
pub(super) struct AutoForward {
    message: Message,
    job: ForwardJob,
    started: std::time::Instant,
}

/// The message a filter sends to forward `m` to `to`: the manual Forward's
/// subject and quoted original, with `attachments` (files on disk) attached.
/// No signature is added and the original's Message-ID goes in `forwards`, so
/// it is marked as forwarded once this has gone.
pub(super) fn forward_outgoing(
    m: &Message,
    to: &[String],
    attachments: Vec<String>,
) -> OutgoingMessage {
    let header = format!(
        "---------- Forwarded message ----------\nFrom: {} <{}>\nDate: {}\nSubject: {}",
        m.from_name, m.from_addr, m.date, m.subject
    );
    let html = crate::mail::markdown::sanitize_outgoing(&quoted_original(&header, &m.body));
    let body = crate::mail::markdown::html_to_text(&html);
    OutgoingMessage {
        from_account_id: m.account_id,
        from_alias: None,
        to: to.join(", "),
        cc: String::new(),
        bcc: String::new(),
        reply_to: String::new(),
        subject: forward_subject(&m.subject),
        body,
        html,
        attachments,
        in_reply_to: String::new(),
        references: String::new(),
        forwards: m.message_id.clone(),
        draft_origin: None,
        outbox_origin: None,
        sign: false,
        encrypt: false,
        send_at: None,
        calendar: None,
    }
}

impl AppModel {
    /// Drop forwards that have waited too long for their message, so a body
    /// that never arrives does not keep the key in flight for good.
    pub(super) fn purge_stale_auto_forwards(&mut self) {
        let stale: Vec<String> = self
            .auto_forwards
            .iter()
            .filter(|f| f.started.elapsed() > AUTO_FORWARD_TIMEOUT)
            .map(|f| f.job.key.clone())
            .collect();
        if stale.is_empty() {
            return;
        }
        tracing::warn!(
            "filter: {} forward(s) gave up waiting for their message",
            stale.len()
        );
        self.auto_forwards.retain(|f| !stale.contains(&f.job.key));
        for key in stale {
            self.auto_forward_inflight.remove(&key);
        }
    }

    /// Start forwarding `m` as `job` says. The message is fetched in full
    /// first when the list only carries its preview.
    pub(super) fn start_auto_forward(&mut self, m: Message, job: ForwardJob) {
        if !self.auto_forward_inflight.insert(job.key.clone()) {
            return;
        }
        tracing::info!(
            "filter: forwarding {}:{} [{}]",
            m.account_id,
            m.id,
            job.rule
        );
        let (account_id, id) = (m.account_id, m.id);
        // A request for this message's body or files is already out when
        // another forward of it is waiting; its answer wakes both.
        let busy = self
            .auto_forwards
            .iter()
            .any(|f| f.message.account_id == account_id && f.message.id == id);
        self.auto_forwards.push(AutoForward {
            message: m,
            job,
            started: std::time::Instant::now(),
        });
        if !busy {
            self.advance_auto_forwards(account_id, id);
        }
    }

    /// A body or the files of message `id` arrived (or turned out not to
    /// exist, `no_attachments`): carry its waiting forwards on.
    pub(super) fn resume_auto_forwards(
        &mut self,
        account_id: u32,
        id: u32,
        key: Option<BodyKey>,
        no_attachments: bool,
    ) {
        // A UID names a message only within its folder: an event about the
        // same number in another folder is not about this message.
        let is_target = |m: &Message| is_event_target(m, key, account_id, id);
        if no_attachments {
            for f in self
                .auto_forwards
                .iter_mut()
                .filter(|f| is_target(&f.message))
            {
                f.message.has_attachment = false;
            }
        }
        let Some(target) = self
            .auto_forwards
            .iter()
            .find(|f| is_target(&f.message))
            .map(|f| (f.message.account_id, f.message.id))
        else {
            return;
        };
        self.advance_auto_forwards(target.0, target.1);
    }

    /// Send the forwards of message `id` if its body and files are in hand;
    /// otherwise ask for what is missing and wait for the answer.
    fn advance_auto_forwards(&mut self, account_id: u32, id: u32) {
        let waiting: Vec<usize> = (0..self.auto_forwards.len())
            .filter(|&i| {
                let m = &self.auto_forwards[i].message;
                m.account_id == account_id && m.id == id
            })
            .collect();
        let Some(&first) = waiting.first() else {
            return;
        };
        let m = self.with_cached_body(self.auto_forwards[first].message.clone());
        if self.fetch_for_copy(&m) {
            return;
        }
        // Nothing to fetch it from and nothing in hand: sending would forward
        // an empty message. The forwards are dropped and their keys stay in
        // flight, so the message stays where it is for this session.
        if m.body.is_empty() {
            tracing::warn!("filter: {}:{} has no body to forward", account_id, id);
            self.auto_forwards
                .retain(|f| !(f.message.account_id == account_id && f.message.id == id));
            return;
        }
        let files: Vec<String> = self
            .attachment_cache
            .get(&body_key(&m))
            .map(|items| stage_attachments(&format!("hylki-autofwd-{account_id}-{id}"), items))
            .unwrap_or_default()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let sending: Vec<AutoForward> = {
            let mut taken = Vec::new();
            let mut i = 0;
            while i < self.auto_forwards.len() {
                let f = &self.auto_forwards[i];
                if f.message.account_id == account_id && f.message.id == id {
                    taken.push(self.auto_forwards.remove(i));
                } else {
                    i += 1;
                }
            }
            taken
        };
        for f in sending {
            self.send_ticket += 1;
            let ticket = self.send_ticket;
            let out = forward_outgoing(&m, &f.job.to, files.clone());
            self.auto_forward_tickets.insert(
                ticket,
                SentForward {
                    key: f.job.key.clone(),
                    rule: f.job.rule.clone(),
                    forwards: out.forwards.clone(),
                    to: out.to.clone(),
                    refused: false,
                },
            );
            self.send_to(
                account_id,
                MailRequest::Send {
                    message: Box::new(out),
                    sent_path: None,
                    ticket,
                },
            );
        }
    }

    /// The worker handed back a message nothing could keep (`Unsent`). When
    /// it is a filter's forward the server refused, it is not forwarded: say
    /// so, and keep it out of the composer. True when it was one.
    pub(super) fn take_refused_auto_forward(&mut self, message: &OutgoingMessage) -> bool {
        let Some(f) = self.auto_forward_tickets.values_mut().find(|f| {
            !f.refused
                && !f.forwards.is_empty()
                && f.forwards == message.forwards
                && f.to == message.to
        }) else {
            return false;
        };
        f.refused = true;
        let rule = f.rule.clone();
        self.notice_error(i18n_f(
            "The forward of filter {rule} was refused. The message was left where it is and will be tried again",
            &[("rule", &rule)],
        ));
        true
    }

    /// A filter's forward was handed to the worker (`sent`, or queued in the
    /// Outbox, which retries it on its own). Either way it is recorded, so
    /// it is never sent a second time, and the filters run again so the
    /// rest of the rule's actions can follow. One the server refused is not
    /// recorded: the message stays held and the next pass tries again.
    pub(super) fn finish_auto_forward(&mut self, ticket: u64, sent: bool) {
        let Some(SentForward {
            key, rule, refused, ..
        }) = self.auto_forward_tickets.remove(&ticket)
        else {
            return;
        };
        self.auto_forward_inflight.remove(&key);
        if refused {
            return;
        }
        self.forwarded_log.record(key);
        if !sent {
            self.notice_error(i18n_f(
                "The forward of filter {rule} is waiting in the Outbox",
                &[("rule", &rule)],
            ));
        }
        self.sweep_blacklisted();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::msg;

    #[test]
    fn subject_gets_fwd_prefix_once() {
        let mut m = msg(1, 1, 1);
        m.subject = "Invoice 12".into();
        let out = forward_outgoing(&m, &["a@x.org".into()], Vec::new());
        assert_eq!(out.subject, "Fwd: Invoice 12");
        m.subject = "Fwd: Invoice 12".into();
        assert_eq!(
            forward_outgoing(&m, &["a@x.org".into()], Vec::new()).subject,
            "Fwd: Invoice 12"
        );
    }

    #[test]
    fn quotes_the_original_and_sets_forwards() {
        let mut m = msg(1, 1, 1);
        m.message_id = "orig@x".into();
        m.body = "Please pay by Friday".into();
        let out = forward_outgoing(&m, &["a@x.org".into()], vec!["/tmp/f.pdf".into()]);
        assert_eq!(out.forwards, "orig@x");
        assert!(out.html.contains("Forwarded message"));
        assert!(out.body.contains("Please pay by Friday"));
        assert_eq!(out.attachments, vec!["/tmp/f.pdf".to_string()]);
        assert!(!out.sign && !out.encrypt && out.send_at.is_none());
    }

    #[test]
    fn recipients_are_joined() {
        let m = msg(1, 1, 1);
        let out = forward_outgoing(&m, &["a@x.org".into(), "b@y.org".into()], Vec::new());
        assert_eq!(out.to, "a@x.org, b@y.org");
    }
}
