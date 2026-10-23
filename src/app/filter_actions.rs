//! What a filter pass does to one message: the forwards still to send, and
//! the mark-as-read, delete or move that follows them. Pure, so the rules of
//! the spec (which forward is skipped, which rule wins) can be tested.

use crate::config::{forward_key, FilterRule, ForwardedLog};
use crate::models::Message;

/// One forward to send: the destinations of one rule for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForwardJob {
    /// Identifies the forward in the [`ForwardedLog`].
    pub(super) key: String,
    /// Destination addresses, lowercased.
    pub(super) to: Vec<String>,
    /// The rule's label, for the log and for error notices.
    pub(super) rule: String,
}

/// What a pass does to one message.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ActionPlan {
    /// Forwards not yet recorded and eligible, one per distinct key.
    pub(super) forwards: Vec<ForwardJob>,
    /// Mark the message read.
    pub(super) mark_read: bool,
    /// Delete the message for good.
    pub(super) delete: bool,
    /// Move the message here.
    pub(super) move_to: Option<String>,
}

/// The reference a forward of `m` is recorded under: its Message-ID, or the
/// folder and UID when it has none.
fn message_ref(m: &Message, src_path: Option<&str>) -> String {
    if m.message_id.is_empty() {
        format!("uid:{}:{}", src_path.unwrap_or(""), m.uid)
    } else {
        m.message_id.clone()
    }
}

/// Whether `rule` may forward `m`: the message arrived after the Forward was
/// added, and it is neither the account's own mail nor from a destination,
/// which is how a forward coming back into a filtered mailbox is told apart.
fn may_forward(rule: &FilterRule, m: &Message, own: &[String]) -> bool {
    let from = m.from_addr.trim();
    if rule.forward_since > 0 && m.timestamp < rule.forward_since {
        return false;
    }
    let same = |a: &String| a.trim().eq_ignore_ascii_case(from);
    !(own.iter().any(same) || rule.forward_to.iter().any(same))
}

/// Plan the actions of the `matching` rules on `m`, found in `src_path`.
/// `own` are the account's addresses, `log` the forwards already done, and
/// `dest_ok` says whether a destination folder exists on the account.
///
/// Every matching rule forwards. The first matching rule that deletes or has
/// a usable destination (other than where the message already is) decides
/// the delete or the move; later ones are ignored.
pub(super) fn plan_actions(
    matching: &[&FilterRule],
    m: &Message,
    src_path: Option<&str>,
    own: &[String],
    log: &ForwardedLog,
    dest_ok: &dyn Fn(&str) -> bool,
) -> ActionPlan {
    let mut plan = ActionPlan::default();
    let reference = message_ref(m, src_path);
    for rule in matching {
        if rule.forward_to.is_empty() || !may_forward(rule, m, own) {
            continue;
        }
        let key = forward_key(&rule.account_email, &reference, &rule.forward_to);
        if log.contains(&key) || plan.forwards.iter().any(|j| j.key == key) {
            continue;
        }
        plan.forwards.push(ForwardJob {
            key,
            to: rule.forward_to.clone(),
            rule: rule.label(),
        });
    }
    plan.mark_read = m.unread && matching.iter().any(|r| r.mark_read);
    let decider = matching.iter().find(|r| {
        r.delete
            || (!r.dest_path.is_empty() && dest_ok(&r.dest_path) && src_path != Some(&r.dest_path))
    });
    if let Some(rule) = decider {
        if rule.delete {
            plan.delete = true;
        } else {
            plan.move_to = Some(rule.dest_path.clone());
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::msg;
    use crate::config::{FilterField, FilterMatch};

    fn rule() -> FilterRule {
        let mut r = FilterRule::draft_from_message("me@x.org", "billing@shop.example", "");
        r.field = FilterField::FromAddress;
        r.matcher = FilterMatch::Equals;
        r.dest_path = String::new();
        r
    }

    fn forwarding(to: &[&str]) -> FilterRule {
        let mut r = rule();
        r.forward_to = to.iter().map(|s| s.to_string()).collect();
        r
    }

    fn plan(rules: &[FilterRule], m: &Message, log: &ForwardedLog) -> ActionPlan {
        let refs: Vec<&FilterRule> = rules.iter().collect();
        plan_actions(&refs, m, Some("INBOX"), &["me@x.org".into()], log, &|_| {
            true
        })
    }

    fn arrived(mut m: Message) -> Message {
        m.message_id = "m1".into();
        m.from_addr = "billing@shop.example".into();
        m.timestamp = 2_000;
        m.unread = true;
        m
    }

    #[test]
    fn a_message_without_an_id_is_forwarded_under_a_fallback_key() {
        let mut m = arrived(msg(1, 1, 7));
        m.message_id = String::new();
        let rules = [forwarding(&["admin@corp.example"])];
        let first = plan(&rules, &m, &ForwardedLog::default());
        assert_eq!(first.forwards.len(), 1);
        assert!(first.forwards[0].key.contains("uid:INBOX:7"));
        let log = ForwardedLog::with_keys(vec![first.forwards[0].key.clone()]);
        assert!(plan(&rules, &m, &log).forwards.is_empty());
    }

    #[test]
    fn duplicate_and_reordered_destinations_forward_once() {
        let m = arrived(msg(1, 1, 1));
        let rules = [
            forwarding(&["a@corp.example", "b@corp.example"]),
            forwarding(&["B@corp.example", "a@corp.example"]),
        ];
        assert_eq!(plan(&rules, &m, &ForwardedLog::default()).forwards.len(), 1);
    }

    #[test]
    fn own_mail_and_mail_from_a_destination_are_not_forwarded() {
        let rules = [forwarding(&["admin@corp.example"])];
        let mut m = arrived(msg(1, 1, 1));
        m.from_addr = "ME@x.org".into();
        assert!(plan(&rules, &m, &ForwardedLog::default())
            .forwards
            .is_empty());
        m.from_addr = "admin@corp.example".into();
        assert!(plan(&rules, &m, &ForwardedLog::default())
            .forwards
            .is_empty());
    }

    #[test]
    fn old_mail_is_not_forwarded_but_keeps_the_other_actions() {
        let mut r = forwarding(&["admin@corp.example"]);
        r.forward_since = 3_000;
        r.mark_read = true;
        let m = arrived(msg(1, 1, 1));
        let p = plan(&[r], &m, &ForwardedLog::default());
        assert!(p.forwards.is_empty());
        assert!(p.mark_read);
    }

    #[test]
    fn mail_with_an_unknown_date_is_not_forwarded_once_a_start_is_set() {
        let mut r = forwarding(&["admin@corp.example"]);
        r.forward_since = 3_000;
        let mut m = arrived(msg(1, 1, 1));
        m.timestamp = 0;
        assert!(plan(&[r], &m, &ForwardedLog::default()).forwards.is_empty());
    }

    #[test]
    fn first_move_or_delete_wins_and_all_forwards_run() {
        let mut mv = rule();
        mv.dest_path = "Admin".into();
        let mut del = rule();
        del.delete = true;
        let fwd = forwarding(&["admin@corp.example"]);
        let m = arrived(msg(1, 1, 1));
        let p = plan(
            &[fwd.clone(), mv.clone(), del.clone()],
            &m,
            &ForwardedLog::default(),
        );
        assert_eq!(p.move_to.as_deref(), Some("Admin"));
        assert!(!p.delete);
        assert_eq!(p.forwards.len(), 1);
        let p = plan(&[del, mv], &m, &ForwardedLog::default());
        assert!(p.delete && p.move_to.is_none());
    }

    #[test]
    fn mark_read_is_skipped_when_already_read() {
        let mut r = rule();
        r.mark_read = true;
        let mut m = arrived(msg(1, 1, 1));
        assert!(plan(&[r.clone()], &m, &ForwardedLog::default()).mark_read);
        m.unread = false;
        assert!(!plan(&[r], &m, &ForwardedLog::default()).mark_read);
    }

    #[test]
    fn a_destination_that_is_gone_or_is_the_source_moves_nothing() {
        let mut r = rule();
        r.dest_path = "INBOX".into();
        let m = arrived(msg(1, 1, 1));
        assert_eq!(
            plan(&[r.clone()], &m, &ForwardedLog::default()).move_to,
            None
        );
        r.dest_path = "Gone".into();
        let refs = [&r];
        let p = plan_actions(
            &refs,
            &m,
            Some("INBOX"),
            &[],
            &ForwardedLog::default(),
            &|_| false,
        );
        assert_eq!(p.move_to, None);
    }
}
