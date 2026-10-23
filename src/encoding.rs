//! Text encodings the app writes and reads: base64 (with the lenient
//! decoder mail needs), percent-encoding, the HTML text escape, and the
//! iCalendar and vCard TEXT escapes. One home for each, so every
//! module encodes the same way.

mod base64;
pub(crate) mod percent;
mod text_value;

pub(crate) use self::base64::{base64_decode, base64_encode, base64_lines};
pub(crate) use text_value::{escape_text_value, unescape_text_value};

/// Text escaped for HTML, for element content and quoted attribute values
/// alike: `&`, `<`, `>`, `'` and `"` become entities, as do the C0 and C1
/// control characters XML forbids (GLib's markup escape).
pub(crate) fn escape_html(text: &str) -> String {
    gtk::glib::markup_escape_text(text).into()
}

#[cfg(test)]
mod tests {
    use super::escape_html;

    /// The corpus the reader, the reader's document, Markdown and the
    /// composer's cloud links were all escaped with before they shared
    /// this function (each was `markup_escape_text(s).into()`).
    #[test]
    fn html_escape_covers_markup_quotes_and_control_characters() {
        let cases = [
            ("", ""),
            ("plain text", "plain text"),
            ("a < b > c", "a &lt; b &gt; c"),
            ("Tom & Jerry", "Tom &amp; Jerry"),
            ("&amp; stays escaped", "&amp;amp; stays escaped"),
            (
                "\"double\" 'single'",
                "&quot;double&quot; &apos;single&apos;",
            ),
            (
                "<script>alert(1)</script>",
                "&lt;script&gt;alert(1)&lt;/script&gt;",
            ),
            ("line\nbreak\ttab\rreturn", "line\nbreak\ttab\rreturn"),
            ("bell\u{7}", "bell&#x7;"),
            ("del\u{7f}c1\u{85}nel", "del&#x7f;c1\u{85}nel"),
            ("Grüße, 日本, 🙂", "Grüße, 日本, 🙂"),
        ];
        for (text, html) in cases {
            assert_eq!(escape_html(text), html, "{text:?}");
        }
    }
}
