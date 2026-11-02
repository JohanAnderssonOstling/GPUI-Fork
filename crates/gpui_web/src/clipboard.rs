use gpui::{ClipboardEntry, ClipboardItem};

/// Preserve an explicit empty string so the platform can clear the clipboard.
pub(crate) fn clipboard_write_text(item: &ClipboardItem) -> Option<String> {
    item.text().or_else(|| {
        item.entries()
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::String(_)))
            .then(String::new)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_a_clipboard_write() {
        assert_eq!(
            clipboard_write_text(&ClipboardItem::new_string(String::new())),
            Some(String::new())
        );
    }

    #[test]
    fn missing_text_is_not_a_clipboard_write() {
        assert_eq!(
            clipboard_write_text(&ClipboardItem {
                entries: Vec::new()
            }),
            None
        );
    }

    #[test]
    fn nonempty_text_is_preserved() {
        assert_eq!(
            clipboard_write_text(&ClipboardItem::new_string("books".into())),
            Some("books".into())
        );
    }
}
