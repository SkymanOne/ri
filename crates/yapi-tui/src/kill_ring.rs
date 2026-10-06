//! Emacs-style kill ring shared by the editor and the single-line input.
//!
//! Port of `packages/tui/src/kill-ring.ts` in pi `v1.0.0`.

/// Killed text, most recent last.
#[derive(Clone, Debug, Default)]
pub(crate) struct KillRing {
    entries: Vec<String>,
}

impl KillRing {
    /// Records killed text. With `accumulate`, it joins the latest entry:
    /// before it when `prepend`, after it otherwise. Empty text is ignored.
    pub(crate) fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        match self.entries.last_mut() {
            Some(last) if accumulate => {
                if prepend {
                    last.insert_str(0, text);
                } else {
                    last.push_str(text);
                }
            }
            _ => self.entries.push(text.to_owned()),
        }
    }

    /// The most recent entry.
    pub(crate) fn peek(&self) -> Option<&str> {
        self.entries.last().map(String::as_str)
    }

    /// Moves the most recent entry to the oldest position.
    pub(crate) fn rotate(&mut self) {
        if let Some(last) = self.entries.pop() {
            self.entries.insert(0, last);
        }
    }

    /// The number of entries.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_rotates() {
        let mut ring = KillRing::default();
        ring.push("world", false, false);
        ring.push("hello ", true, true);
        assert_eq!(ring.peek(), Some("hello world"));
        ring.push("other", false, false);
        ring.rotate();
        assert_eq!(ring.peek(), Some("hello world"));
        assert_eq!(ring.len(), 2);
    }
}
