use zeroize::Zeroizing;

use crate::credentials::SecretValue;

const REPLACEMENT: &[u8] = b"[REDACTED]";
const MINIMUM_PATTERN_BYTES: usize = 6;

#[derive(Default)]
pub struct Redactor {
    patterns: Vec<Zeroizing<Vec<u8>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RedactionMetadata {
    pub registered_patterns: usize,
}

impl Redactor {
    pub fn register(&mut self, secret: SecretValue) {
        let bytes = secret.expose().as_bytes();
        if bytes.len() < MINIMUM_PATTERN_BYTES
            || self
                .patterns
                .iter()
                .any(|pattern| pattern.as_slice() == bytes)
        {
            return;
        }
        self.patterns.push(Zeroizing::new(bytes.to_vec()));
        self.patterns
            .sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
    }

    pub fn redact(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut offset = 0;
        while offset < text.len() {
            let remaining = &text[offset..];
            let matched = self
                .patterns
                .iter()
                .find(|pattern| remaining.as_bytes().starts_with(pattern.as_slice()))
                .map(|pattern| pattern.len());
            if let Some(length) = matched {
                output.push_str("[REDACTED]");
                offset += length;
            } else {
                let character = remaining.chars().next().expect("non-empty remainder");
                output.push(character);
                offset += character.len_utf8();
            }
        }
        output
    }

    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(bytes.len());
        let mut offset = 0;
        while offset < bytes.len() {
            if let Some(pattern) = self
                .patterns
                .iter()
                .find(|pattern| bytes[offset..].starts_with(pattern.as_slice()))
            {
                output.extend_from_slice(REPLACEMENT);
                offset += pattern.len();
            } else {
                output.push(bytes[offset]);
                offset += 1;
            }
        }
        output
    }

    pub fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_json(value);
                }
            }
            serde_json::Value::Object(values) => {
                let original = std::mem::take(values);
                let mut renamed = Vec::new();
                for (key, mut value) in original {
                    self.redact_json(&mut value);
                    let redacted = self.redact(&key);
                    if redacted == key {
                        values.insert(key, value);
                    } else {
                        renamed.push((redacted, value));
                    }
                }
                // Reserve unchanged keys first, including literal redaction markers.
                for (key, value) in renamed {
                    let mut unique = key.clone();
                    let mut suffix = 2;
                    while values.contains_key(&unique) {
                        unique = format!("{key}#{suffix}");
                        suffix += 1;
                    }
                    values.insert(unique, value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    pub fn safe_metadata(&self) -> RedactionMetadata {
        RedactionMetadata {
            registered_patterns: self.patterns.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn colliding_secret_keys_retain_every_value_and_literal_marker_key() {
        let mut redactor = Redactor::default();
        redactor.register(SecretValue::new("secret-alpha".into()));
        redactor.register(SecretValue::new("secret-bravo".into()));
        let mut value = json!({
            "secret-alpha": {"nested": "secret-bravo"},
            "secret-bravo": 2,
            "[REDACTED]": 3,
            "[REDACTED]#2": null,
            "nested": [{"secret-alpha": 4, "secret-bravo": 5}],
        });
        redactor.redact_json(&mut value);
        assert_eq!(
            value,
            json!({
                "[REDACTED]": 3,
                "[REDACTED]#2": null,
                "[REDACTED]#3": {"nested": "[REDACTED]"},
                "[REDACTED]#4": 2,
                "nested": [{"[REDACTED]": 4, "[REDACTED]#2": 5}],
            })
        );
        let serialized = value.to_string();
        assert!(!serialized.contains("secret-alpha"));
        assert!(!serialized.contains("secret-bravo"));
        let once = value.clone();
        redactor.redact_json(&mut value);
        assert_eq!(value, once);
    }

    #[test]
    fn overlapping_unicode_patterns_match_longest_in_text_and_bytes() {
        let mut redactor = Redactor::default();
        redactor.register(SecretValue::new("秘密-token".into()));
        redactor.register(SecretValue::new("秘密-token-long".into()));
        let text = "é秘密-token-long/秘密-token終";
        let expected = "é[REDACTED]/[REDACTED]終";
        assert_eq!(redactor.redact(text), expected);
        assert_eq!(redactor.redact_bytes(text.as_bytes()), expected.as_bytes());
    }
}
