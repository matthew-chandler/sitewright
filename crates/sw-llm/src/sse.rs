//! A small server-sent events parser.
//!
//! Network chunks split the stream at arbitrary byte positions: in the middle of a line,
//! of a multi-byte character, or of a CRLF pair. [`SseParser`] buffers bytes and yields
//! only complete events.

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` lines, joined with newlines.
    pub data: String,
}

/// Incremental parser: feed it bytes, get back the events they complete.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a chunk and return every event it completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buffer[start..].iter().position(|b| *b == b'\n') {
            let end = start + offset;
            let mut line = &self.buffer[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            start = end + 1;
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        self.buffer.drain(..start);
        events
    }

    /// Signal the end of the stream. Returns a final event if one was left unterminated.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            let line = line.trim_end_matches('\r');
            if let Some(event) = self.line(line) {
                return Some(event);
            }
        }
        self.line("")
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            // A blank line ends the event.
            if self.data.is_empty() && self.event.is_none() {
                return None;
            }
            return Some(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        if line.starts_with(':') {
            return None; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            _ => {} // id, retry and unknown fields are not needed
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = "event: start\ndata: {\"a\":1}\n\n: keep-alive\n\ndata: line one\ndata: line two\r\n\r\ndata:nospace\n\ndata: caf\u{e9} \u{1f600}\n\n";

    fn expected() -> Vec<SseEvent> {
        vec![
            SseEvent {
                event: Some("start".into()),
                data: "{\"a\":1}".into(),
            },
            SseEvent {
                event: None,
                data: "line one\nline two".into(),
            },
            SseEvent {
                event: None,
                data: "nospace".into(),
            },
            SseEvent {
                event: None,
                data: "caf\u{e9} \u{1f600}".into(),
            },
        ]
    }

    #[test]
    fn parses_a_whole_stream() {
        let mut parser = SseParser::new();
        assert_eq!(parser.feed(STREAM.as_bytes()), expected());
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn any_chunk_boundary_gives_the_same_events() {
        let bytes = STREAM.as_bytes();
        // Every possible split point, including inside multi-byte characters and CRLF.
        for split in 0..=bytes.len() {
            let mut parser = SseParser::new();
            let mut events = parser.feed(&bytes[..split]);
            events.extend(parser.feed(&bytes[split..]));
            assert_eq!(events, expected(), "split at {split}");
        }
        // One byte at a time.
        let mut parser = SseParser::new();
        let events: Vec<_> = bytes.iter().flat_map(|b| parser.feed(&[*b])).collect();
        assert_eq!(events, expected());
    }

    #[test]
    fn finish_flushes_an_unterminated_event() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: tail").is_empty());
        assert_eq!(
            parser.finish(),
            Some(SseEvent {
                event: None,
                data: "tail".into()
            })
        );
        assert_eq!(parser.finish(), None);
    }
}
