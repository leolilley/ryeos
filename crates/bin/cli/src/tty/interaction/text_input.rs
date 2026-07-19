use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use super::{Key, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputAction {
    Unchanged,
    Changed,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextInput {
    value: String,
    char_count: usize,
    max_chars: usize,
}

impl TextInput {
    pub(crate) fn new(max_chars: usize) -> Self {
        Self {
            value: String::new(),
            char_count: 0,
            max_chars,
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.value.clear();
        self.char_count = 0;
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> InputAction {
        match event.key {
            Key::Enter => InputAction::Submit,
            Key::Escape => InputAction::Cancel,
            Key::Backspace => {
                if self.value.pop().is_some() {
                    self.char_count = self.char_count.saturating_sub(1);
                    InputAction::Changed
                } else {
                    InputAction::Unchanged
                }
            }
            Key::Char(value) if !event.modifiers.control && !value.is_control() => self.push(value),
            _ => InputAction::Unchanged,
        }
    }

    pub(crate) fn paste(&mut self, value: &str) -> InputAction {
        let before = self.char_count;
        for value in value.chars().filter(|value| !value.is_control()) {
            if self.char_count == self.max_chars {
                break;
            }
            self.value.push(value);
            self.char_count += 1;
        }
        if self.char_count == before {
            InputAction::Unchanged
        } else {
            InputAction::Changed
        }
    }

    fn push(&mut self, value: char) -> InputAction {
        if self.char_count >= self.max_chars {
            return InputAction::Unchanged;
        }
        self.value.push(value);
        self.char_count += 1;
        InputAction::Changed
    }
}

/// A bounded input buffer whose contents are never exposed through Debug.
pub(crate) struct SecretInput {
    value: Zeroizing<String>,
    char_count: usize,
    max_chars: usize,
}

impl SecretInput {
    pub(crate) fn new(max_chars: usize) -> Self {
        Self {
            value: Zeroizing::new(String::new()),
            char_count: 0,
            max_chars,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> InputAction {
        match event.key {
            Key::Enter => InputAction::Submit,
            Key::Escape => InputAction::Cancel,
            Key::Backspace => {
                if secret_pop(&mut self.value) {
                    self.char_count = self.char_count.saturating_sub(1);
                    InputAction::Changed
                } else {
                    InputAction::Unchanged
                }
            }
            Key::Char(value) if !event.modifiers.control && !value.is_control() => self.push(value),
            _ => InputAction::Unchanged,
        }
    }

    pub(crate) fn paste(&mut self, value: &str) -> InputAction {
        let before = self.char_count;
        for value in value.chars().filter(|value| !value.is_control()) {
            if self.char_count == self.max_chars {
                break;
            }
            self.value.push(value);
            self.char_count += 1;
        }
        if self.char_count == before {
            InputAction::Unchanged
        } else {
            InputAction::Changed
        }
    }

    /// Move the secret into a zeroizing owner for an async operation.
    ///
    /// Async callers take ownership; both the old and returned allocations are
    /// wiped on drop.
    pub(crate) fn take_secret(&mut self) -> Zeroizing<String> {
        self.char_count = 0;
        std::mem::replace(&mut self.value, Zeroizing::new(String::new()))
    }

    fn push(&mut self, value: char) -> InputAction {
        if self.char_count >= self.max_chars {
            return InputAction::Unchanged;
        }
        self.value.push(value);
        self.char_count += 1;
        InputAction::Changed
    }
}

fn secret_pop(value: &mut Zeroizing<String>) -> bool {
    let Some((new_len, _)) = value.char_indices().next_back() else {
        return false;
    };
    // SAFETY: `new_len` came from a UTF-8 character boundary. The removed
    // suffix is wiped while the bytes are viewed as a Vec, then the length is
    // restored to that valid boundary before the String is observed again.
    unsafe {
        let bytes = value.as_mut_vec();
        bytes[new_len..].zeroize();
        bytes.set_len(new_len);
    }
    true
}

impl fmt::Debug for SecretInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretInput([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_input_is_bounded_by_unicode_scalars() {
        let mut input = TextInput::new(2);
        assert_eq!(input.paste("å界x"), InputAction::Changed);
        assert_eq!(input.value(), "å界");
        assert_eq!(
            input.handle_key(KeyEvent::plain(Key::Backspace)),
            InputAction::Changed
        );
        assert_eq!(input.value(), "å");
    }

    #[test]
    fn pasted_line_endings_are_not_inserted() {
        let mut input = TextInput::new(20);
        input.paste("first\nsecond\r");
        assert_eq!(input.value(), "firstsecond");
    }

    #[test]
    fn secret_debug_never_reveals_value_or_length() {
        let mut input = SecretInput::new(100);
        input.paste("extremely-sensitive-value");
        assert_eq!(format!("{input:?}"), "SecretInput([REDACTED])");
        let secret = input.take_secret();
        assert!(secret.starts_with("extremely"));
        drop(secret);
        assert!(input.is_empty());
    }
}
