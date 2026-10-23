//! The log of messages a filter has already forwarded, so that no message is
//! forwarded twice however often the filters run over it. Kept in its own
//! file, not in the cache, which can be wiped at any time: a wipe would
//! forward everything again.

use super::{config_file, read_toml, save_private_toml};

/// How many forwards the log remembers; the oldest go first.
const MAX_FORWARDED_KEYS: usize = 5000;

/// The key of one forward: the account, the message and the destinations.
/// `message_ref` is the Message-ID, or a stand-in when the message has none.
/// The destinations are sorted and lowercased, so reordering the addresses of
/// a rule does not forward the same message again, while changing them does.
pub(crate) fn forward_key(account_email: &str, message_ref: &str, to: &[String]) -> String {
    let mut to: Vec<String> = to.iter().map(|a| a.to_lowercase()).collect();
    to.sort();
    format!(
        "{}|{}|{}",
        account_email.to_lowercase(),
        message_ref,
        to.join(",")
    )
}

/// What `filter_forwarded.toml` holds.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ForwardedFile {
    #[serde(default)]
    keys: Vec<String>,
}

/// The forwards done so far, oldest first.
#[derive(Default)]
pub(crate) struct ForwardedLog {
    keys: Vec<String>,
}

/// Append `key` to `keys`, dropping the oldest entries beyond the cap.
fn push_capped(keys: &mut Vec<String>, key: String, cap: usize) {
    keys.push(key);
    if keys.len() > cap {
        let extra = keys.len() - cap;
        keys.drain(..extra);
    }
}

impl ForwardedLog {
    /// The log as it is on disk; empty when there is none yet.
    pub(crate) fn load() -> Self {
        let keys = read_toml::<ForwardedFile>(config_file("filter_forwarded.toml"))
            .map(|f| f.keys)
            .unwrap_or_default();
        ForwardedLog { keys }
    }

    /// Whether this forward was already done.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    /// Remember a forward, and save the log.
    pub(crate) fn record(&mut self, key: String) {
        if self.contains(&key) {
            return;
        }
        push_capped(&mut self.keys, key, MAX_FORWARDED_KEYS);
        let file = ForwardedFile {
            keys: self.keys.clone(),
        };
        save_private_toml(
            config_file("filter_forwarded.toml"),
            &file,
            "forwarded messages",
            true,
        );
    }

    /// A log holding `keys`, for tests.
    #[cfg(test)]
    pub(crate) fn with_keys(keys: Vec<String>) -> Self {
        ForwardedLog { keys }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_destination_order_and_case() {
        let a = forward_key("Me@X.org", "<m1>", &["b@y.org".into(), "A@y.org".into()]);
        let b = forward_key("me@x.org", "<m1>", &["a@y.org".into(), "B@Y.org".into()]);
        assert_eq!(a, b);
        assert_ne!(a, forward_key("me@x.org", "<m1>", &["a@y.org".into()]));
        assert_ne!(
            a,
            forward_key("me@x.org", "<m2>", &["a@y.org".into(), "b@y.org".into()])
        );
    }

    #[test]
    fn contains_finds_a_recorded_key() {
        let log = ForwardedLog::with_keys(vec!["k1".into()]);
        assert!(log.contains("k1") && !log.contains("k2"));
    }

    #[test]
    fn log_is_capped_oldest_first() {
        let mut keys: Vec<String> = (0..3).map(|i| format!("k{i}")).collect();
        push_capped(&mut keys, "k3".into(), 3);
        assert_eq!(keys, vec!["k1", "k2", "k3"]);
    }
}
