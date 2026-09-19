use kurama_protocol::KuramaError;

const MAX_SSE_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Incremental SSE decoding with a 1 MiB limit on each wire record.
#[derive(Debug)]
pub struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    id: Option<String>,
    has_data: bool,
    first_line: bool,
    skip_lf: bool,
    record_bytes: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            event: None,
            data: String::new(),
            id: None,
            has_data: false,
            first_line: true,
            skip_lf: false,
            record_bytes: 0,
        }
    }
}

impl SseDecoder {
    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<SseEvent>, KuramaError> {
        let mut events = Vec::new();
        while !chunk.is_empty() {
            let (consumed, event) = self.push_chunk(chunk)?;
            chunk = &chunk[consumed..];
            if let Some(event) = event {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, KuramaError> {
        Ok(self.finish_event()?.into_iter().collect())
    }

    // Consume only through the next event so the transport can retain its Bytes
    // without copying or eagerly normalizing a whole network chunk.
    pub(crate) fn push_chunk(
        &mut self,
        chunk: &[u8],
    ) -> Result<(usize, Option<SseEvent>), KuramaError> {
        let mut start = 0;
        for (index, byte) in chunk.iter().copied().enumerate() {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    if self.record_bytes != 0 {
                        self.record_bytes += 1;
                        self.check_size()?;
                    }
                    start = index + 1;
                    continue;
                }
            }
            self.record_bytes += 1;
            self.check_size()?;
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let event = self.finish_line(&chunk[start..index])?;
                start = index + 1;
                if let Some(event) = event {
                    return Ok((start, Some(event)));
                }
            }
        }
        self.line.extend_from_slice(&chunk[start..]);
        Ok((chunk.len(), None))
    }

    pub(crate) fn finish_event(&mut self) -> Result<Option<SseEvent>, KuramaError> {
        if !self.line.is_empty() {
            self.finish_line(&[])?;
        }
        Ok(self.dispatch())
    }

    fn check_size(&self) -> Result<(), KuramaError> {
        if self.record_bytes > MAX_SSE_RECORD_BYTES {
            return Err(KuramaError::Protocol(format!(
                "SSE record exceeds {MAX_SSE_RECORD_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn finish_line(&mut self, suffix: &[u8]) -> Result<Option<SseEvent>, KuramaError> {
        if self.line.is_empty() {
            return self.parse_line(suffix);
        }
        self.line.extend_from_slice(suffix);
        let mut line = std::mem::take(&mut self.line);
        let event = self.parse_line(&line)?;
        line.clear();
        self.line = line;
        Ok(event)
    }

    fn parse_line(&mut self, line: &[u8]) -> Result<Option<SseEvent>, KuramaError> {
        let line = std::str::from_utf8(line)
            .map_err(|error| KuramaError::Protocol(format!("SSE is not UTF-8: {error}")))?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            _ => {}
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        self.record_bytes = 0;
        let event = self.event.take();
        let id = self.id.take();
        if !std::mem::take(&mut self.has_data) {
            return None;
        }
        Some(SseEvent {
            event,
            data: std::mem::take(&mut self.data),
            id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_and_mixed_line_endings_survive_arbitrary_fragmentation() {
        let wire =
            "\u{feff}: keepalive\revent: update\rdata: α\r\nid: 7\ndata: β\r\rdata: last\n\n";
        for size in [1, 2, 7, wire.len()] {
            let mut decoder = SseDecoder::default();
            let mut events = Vec::new();
            for chunk in wire.as_bytes().chunks(size) {
                events.extend(decoder.push(chunk).expect("fragment"));
            }
            events.extend(decoder.finish().expect("finish"));
            assert_eq!(
                events,
                vec![
                    SseEvent {
                        event: Some("update".into()),
                        data: "α\nβ".into(),
                        id: Some("7".into())
                    },
                    SseEvent {
                        event: None,
                        data: "last".into(),
                        id: None
                    },
                ]
            );
        }
    }

    #[test]
    fn public_decoder_bounds_each_record_including_ignored_fields() {
        let wire = format!("data: {}\n\n", "x".repeat(MAX_SSE_RECORD_BYTES - 8));
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.push(wire.as_bytes()).expect("at limit")[0]
                .data
                .len(),
            MAX_SSE_RECORD_BYTES - 8
        );
        assert_eq!(
            decoder.push(wire.as_bytes()).expect("next record")[0]
                .data
                .len(),
            MAX_SSE_RECORD_BYTES - 8
        );
        let mut decoder = SseDecoder::default();
        let oversized = format!(":{}", "x".repeat(MAX_SSE_RECORD_BYTES));
        assert!(matches!(
            decoder.push(oversized.as_bytes()),
            Err(KuramaError::Protocol(_))
        ));
    }

    #[test]
    fn finish_dispatches_unterminated_data_only_once() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: remaining").expect("push").is_empty());
        assert_eq!(
            decoder.finish().expect("finish"),
            vec![SseEvent {
                event: None,
                data: "remaining".into(),
                id: None
            }]
        );
        assert!(decoder.finish().expect("second finish").is_empty());
    }

    #[test]
    fn invalid_utf8_is_rejected_on_line_boundary_or_eof() {
        for wire in [b"data: \xff\n\n".as_slice(), b"data: \xc3".as_slice()] {
            let mut decoder = SseDecoder::default();
            let result = decoder.push(wire).and_then(|_| decoder.finish());
            assert!(matches!(result, Err(KuramaError::Protocol(_))));
        }
    }
}
