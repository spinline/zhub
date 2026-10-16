//! Logging: stderr (the service redirects it to a file) plus the recent lines in memory
//! for the UI's log viewer. The filter can be changed while running.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, reload};

/// Lines kept for the UI
const KEEP_LINES: usize = 1000;

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub id: u64,
    pub text: String,
}

/// The kept lines and the last id given (32-bit MIPS has no 64-bit atomics)
#[derive(Default)]
pub struct Buffer {
    lines: Mutex<(VecDeque<Line>, u64)>,
}

impl Buffer {
    fn push(&self, text: &str) {
        let mut guard = self.lines.lock().unwrap();
        let (lines, last_id) = &mut *guard;
        for text in text.lines().filter(|l| !l.is_empty()) {
            *last_id += 1;
            let id = *last_id;
            if lines.len() == KEEP_LINES {
                lines.pop_front();
            }
            lines.push_back(Line { id, text: text.to_owned() });
        }
    }

    /// Lines after `after` (0: all kept lines), at most `limit` of the newest
    pub fn since(&self, after: u64, limit: usize) -> Vec<Line> {
        let guard = self.lines.lock().unwrap();
        let newer: Vec<_> = guard.0.iter().filter(|l| l.id > after).cloned().collect();
        let skip = newer.len().saturating_sub(limit);
        newer.into_iter().skip(skip).collect()
    }
}

/// Writes each event to stderr and, once complete, to the buffer
struct Tee {
    buffer: Arc<Buffer>,
}

struct TeeWriter {
    buffer: Arc<Buffer>,
    text: Vec<u8>,
}

impl Write for TeeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.text.extend_from_slice(data);
        io::stderr().write_all(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

impl Drop for TeeWriter {
    fn drop(&mut self) {
        self.buffer.push(&String::from_utf8_lossy(&self.text));
    }
}

impl<'a> MakeWriter<'a> for Tee {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            buffer: self.buffer.clone(),
            text: Vec::new(),
        }
    }
}

pub struct Logging {
    pub buffer: Arc<Buffer>,
    filter: reload::Handle<EnvFilter, Registry>,
    current: Mutex<String>,
}

impl Logging {
    /// Installs the global subscriber with the given filter (e.g. "info,ziggurat_driver=warn")
    pub fn init(filter: &str, ansi: bool) -> Result<Arc<Self>> {
        let parsed = EnvFilter::try_new(filter).with_context(|| format!("invalid log level: {filter}"))?;
        let (layer, handle) = reload::Layer::new(parsed);
        let buffer = Arc::new(Buffer::default());
        tracing_subscriber::registry()
            .with(layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(ansi)
                    .with_writer(Tee { buffer: buffer.clone() }),
            )
            .init();
        Ok(Arc::new(Self {
            buffer,
            filter: handle,
            current: Mutex::new(filter.to_owned()),
        }))
    }

    pub fn level(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    pub fn set_level(&self, filter: &str) -> Result<()> {
        let parsed = EnvFilter::try_new(filter).with_context(|| format!("invalid log level: {filter}"))?;
        self.filter.reload(parsed).context("cannot change the log level")?;
        *self.current.lock().unwrap() = filter.to_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_keeps_the_newest_lines() {
        let buffer = Buffer::default();
        for i in 0..KEEP_LINES + 5 {
            buffer.push(&format!("line {i}\n"));
        }
        let all = buffer.since(0, usize::MAX);
        assert_eq!(all.len(), KEEP_LINES);
        assert_eq!(all[0].text, "line 5");
        let last = all.last().unwrap().id;
        assert!(buffer.since(last, 10).is_empty());
        assert_eq!(buffer.since(last - 2, 10).len(), 2);
        // A limit returns the newest lines
        assert_eq!(buffer.since(0, 3)[2].id, last);
    }

    #[test]
    fn one_write_may_hold_several_lines() {
        let buffer = Buffer::default();
        buffer.push("a\nb\n\nc");
        let texts: Vec<_> = buffer.since(0, 10).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }
}
