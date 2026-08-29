use kurama_protocol::KuramaError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, KuramaError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, separator_len)) = event_boundary(&self.buffer) {
            let record = self.buffer.drain(..end).collect::<Vec<_>>();
            self.buffer.drain(..separator_len);
            if let Some(event) = parse_record(&record)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, KuramaError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let record = std::mem::take(&mut self.buffer);
        Ok(parse_record(&record)?.into_iter().collect())
    }
}

fn event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if buffer[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn parse_record(record: &[u8]) -> Result<Option<SseEvent>, KuramaError> {
    let record = std::str::from_utf8(record)
        .map_err(|error| KuramaError::Protocol(format!("SSE is not UTF-8: {error}")))?;
    let mut event = None;
    let mut id = None;
    let mut data = Vec::new();
    for raw_line in record.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            "id" => id = Some(value.to_owned()),
            _ => {}
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
        id,
    }))
}
