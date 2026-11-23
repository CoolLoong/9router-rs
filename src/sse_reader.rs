use std::time::Duration;
use tokio::time::Instant;

pub struct SseReader {
    upstream: reqwest::Response,
    pending: Vec<u8>,
    offset: usize,
    scan: usize,
    first_line: bool,
    eof: bool,
    max_line: usize,
}

pub enum ReadLine {
    Line(String, bool),
    End,
    Timeout,
    Disconnected,
}

impl SseReader {
    pub fn new(upstream: reqwest::Response) -> Self {
        Self {
            upstream,
            pending: Vec::new(),
            offset: 0,
            scan: 0,
            first_line: true,
            eof: false,
            max_line: usize::MAX,
        }
    }

    pub fn with_line_limit(mut self, max_line: usize) -> Self {
        self.max_line = max_line;
        self
    }

    pub async fn next(&mut self, stall: Duration, deadline: Option<Instant>) -> ReadLine {
        loop {
            if let Some(index) = self.pending[self.scan..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = self.scan + index;
                if end - self.offset > self.max_line {
                    return ReadLine::Disconnected;
                }
                let line = String::from_utf8_lossy(&self.pending[self.offset..end]).into_owned();
                self.offset = end + 1;
                self.scan = self.offset;
                return ReadLine::Line(self.strip_bom(line), true);
            }
            if self.offset != 0 {
                self.pending.drain(..self.offset);
                self.offset = 0;
            }
            if self.pending.len() > self.max_line {
                return ReadLine::Disconnected;
            }
            self.scan = self.pending.len();
            if self.eof {
                if self.pending.is_empty() {
                    return ReadLine::End;
                }
                let tail = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
                self.scan = 0;
                return ReadLine::Line(self.strip_bom(tail), false);
            }
            let timeout = deadline.map_or(stall, |end| {
                stall.min(end.saturating_duration_since(Instant::now()))
            });
            if timeout.is_zero() {
                return ReadLine::Timeout;
            }
            match tokio::time::timeout(timeout, self.upstream.chunk()).await {
                Ok(Ok(Some(chunk))) => self.pending.extend_from_slice(&chunk),
                Ok(Ok(None)) => self.eof = true,
                Ok(Err(_)) => return ReadLine::Disconnected,
                Err(_) => return ReadLine::Timeout,
            }
        }
    }

    fn strip_bom(&mut self, mut line: String) -> String {
        if std::mem::take(&mut self.first_line) && line.starts_with('\u{feff}') {
            line.drain(..'\u{feff}'.len_utf8());
        }
        line
    }
}
