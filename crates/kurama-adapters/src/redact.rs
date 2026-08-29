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
            let matched = self.patterns.iter().find_map(|pattern| {
                std::str::from_utf8(pattern)
                    .ok()
                    .filter(|pattern| remaining.starts_with(pattern))
                    .map(str::len)
            });
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
                for (key, mut value) in original {
                    self.redact_json(&mut value);
                    values.insert(self.redact(&key), value);
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
