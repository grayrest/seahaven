//! Per-job output buffering (D28) and the session's stdin (D36).
//!
//! # Why a segment log, and not two buffers or one
//!
//! D28 wants two things that pull against each other. just's command echo goes
//! to stderr and its output to stdout, and when a human reads the run the echo
//! must sit *attached* to the output it announces — so render order across the
//! two streams matters. But D28's own correction (via D36) says the streams
//! stay **separate on the wire**, because the compatibility suite compares
//! stdout and stderr as distinct byte strings and a merged buffer would make
//! that comparison unsatisfiable.
//!
//! Two independent buffers lose the render order. One merged buffer loses the
//! separation. An ordered log of `(stream, bytes)` segments keeps both: filter
//! by stream to recover each side exactly, or replay in order to get the merge a
//! host renders for display. The merge is thus a *reading* of the log, never a
//! thing done to the bytes — which is D28's "the merge is a rendering decision"
//! made literal.
//!
//! # Why the guest cannot defeat it
//!
//! The stdio effects append to an [`OutputLog`]; there is no method that hands
//! back a raw descriptor, and the guest never holds the host's stdout. So "the
//! guest does not see and must not be able to defeat" the buffering (the plan's
//! words) is a property of the type, not a rule enforced at runtime: the only
//! way out of the log is the host draining it.
//!
//! # Per-job
//!
//! One [`OutputLog`] per session. A session is one job today; when D25 splits a
//! parallel invocation into a store per sub-invocation, each gets its own log,
//! which is per-job by construction. No scheduler is needed for the buffer to be
//! correct — only for there to be more than one at a time.

/// A standard output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One contiguous write to one stream, in the order it happened.
#[derive(Debug, Clone)]
struct Segment {
    stream: Stream,
    bytes: Vec<u8>,
}

/// A job's captured output — separate on the wire, ordered for render (D28).
#[derive(Debug, Default)]
pub struct OutputLog {
    segments: Vec<Segment>,
}

impl OutputLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// Appends bytes written to `stream`.
    ///
    /// Coalesces with the previous segment when it is the same stream, so a log
    /// of many small writes to one stream does not become a segment per write.
    /// The observable result — [`stdout`](Self::stdout), [`stderr`](Self::stderr)
    /// and [`rendered`](Self::rendered) — is identical either way; coalescing
    /// only keeps the log small.
    pub fn write(&mut self, stream: Stream, bytes: &[u8]) {
        if let Some(last) = self.segments.last_mut()
            && last.stream == stream
        {
            last.bytes.extend_from_slice(bytes);
            return;
        }
        self.segments.push(Segment {
            stream,
            bytes: bytes.to_vec(),
        });
    }

    /// Everything written to stdout, in order, with stderr removed.
    #[must_use]
    pub fn stdout(&self) -> Vec<u8> {
        self.collect(Stream::Stdout)
    }

    /// Everything written to stderr, in order, with stdout removed.
    #[must_use]
    pub fn stderr(&self) -> Vec<u8> {
        self.collect(Stream::Stderr)
    }

    /// Both streams interleaved in write order — the host's merge for display.
    ///
    /// This is the *only* place the two streams meet, and it meets them by
    /// reading the log rather than by having merged the bytes. A host that shows
    /// a human one terminal renders this; a comparison that must keep the
    /// streams apart reads [`stdout`](Self::stdout) and [`stderr`](Self::stderr).
    #[must_use]
    pub fn rendered(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for segment in &self.segments {
            out.extend_from_slice(&segment.bytes);
        }
        out
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.iter().all(|s| s.bytes.is_empty())
    }

    fn collect(&self, stream: Stream) -> Vec<u8> {
        let mut out = Vec::new();
        for segment in &self.segments {
            if segment.stream == stream {
                out.extend_from_slice(&segment.bytes);
            }
        }
        out
    }
}

/// The bytes a guest reads from stdin, with a cursor.
///
/// A *fixed source*, not a live terminal: D36 removes the terminal, so there is
/// nothing to read interactively. stdin is whatever was piped to the job, and
/// end-of-file once it is drained. A guest that blocks waiting for more input
/// under a sandbox would block forever, so there is no more to wait for.
#[derive(Debug, Default)]
pub struct StdinSource {
    bytes: Vec<u8>,
    cursor: usize,
}

impl StdinSource {
    /// A source over `bytes`.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, cursor: 0 }
    }

    /// Reads one line, without its trailing newline, or `None` at end of file.
    ///
    /// A `\r\n` and a bare `\n` both terminate a line and are both stripped, so
    /// a justfile does not see a carriage return a Windows pipe left behind. The
    /// final line of input with no trailing newline is still returned; `None`
    /// means the cursor is genuinely at the end.
    pub fn read_line(&mut self) -> Option<String> {
        if self.cursor >= self.bytes.len() {
            return None;
        }
        let rest = &self.bytes[self.cursor..];
        let (line_bytes, advance) = match rest.iter().position(|&b| b == b'\n') {
            Some(nl) => (&rest[..nl], nl + 1),
            None => (rest, rest.len()),
        };
        self.cursor += advance;
        let mut line = String::from_utf8_lossy(line_bytes).into_owned();
        if line.ends_with('\r') {
            line.pop();
        }
        Some(line)
    }

    /// Reads and consumes everything remaining.
    pub fn read_to_end(&mut self) -> Vec<u8> {
        let rest = self.bytes[self.cursor..].to_vec();
        self.cursor = self.bytes.len();
        rest
    }

    /// Reads the next chunk of bytes, or `None` at end of file.
    ///
    /// `basic-cli`'s `stdin_bytes!` reads a fixed-size chunk off a live
    /// descriptor and the caller loops until end of file. This source is fixed,
    /// so one chunk is the whole remainder — the loop still terminates, seeing
    /// the bytes once and then `None`, which is the contract that matters.
    pub fn read_chunk(&mut self) -> Option<Vec<u8>> {
        if self.cursor >= self.bytes.len() {
            return None;
        }
        Some(self.read_to_end())
    }
}

/// A session's three standard streams: the output log and the stdin source.
#[derive(Debug, Default)]
pub struct Stdio {
    /// The captured output.
    pub output: OutputLog,
    /// The stdin source.
    pub input: StdinSource,
}

impl Stdio {
    /// A session with the given stdin and an empty output log.
    #[must_use]
    pub const fn new(stdin: Vec<u8>) -> Self {
        Self {
            output: OutputLog::new(),
            input: StdinSource::new(stdin),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_separate_on_the_wire_but_ordered_for_render() {
        // just's shape: the command echo to stderr, then the command's output to
        // stdout. The compat suite must see them apart; a human must see them
        // attached.
        let mut log = OutputLog::new();
        log.write(Stream::Stderr, b"+ echo hi\n");
        log.write(Stream::Stdout, b"hi\n");
        log.write(Stream::Stderr, b"+ echo bye\n");
        log.write(Stream::Stdout, b"bye\n");

        assert_eq!(log.stdout(), b"hi\nbye\n", "stdout must not carry the echo");
        assert_eq!(
            log.stderr(),
            b"+ echo hi\n+ echo bye\n",
            "stderr must not carry the output"
        );
        assert_eq!(
            log.rendered(),
            b"+ echo hi\nhi\n+ echo bye\nbye\n",
            "the render must keep each echo attached to its output"
        );
    }

    #[test]
    fn coalescing_does_not_change_what_is_observed() {
        let mut one = OutputLog::new();
        one.write(Stream::Stdout, b"abc");
        let mut many = OutputLog::new();
        many.write(Stream::Stdout, b"a");
        many.write(Stream::Stdout, b"b");
        many.write(Stream::Stdout, b"c");
        assert_eq!(one.stdout(), many.stdout());
        assert_eq!(one.rendered(), many.rendered());
    }

    #[test]
    fn stdin_reads_lines_then_reports_end_of_file() {
        let mut input = StdinSource::new(b"first\r\nsecond\nlast".to_vec());
        assert_eq!(input.read_line().as_deref(), Some("first"), "strips \\r\\n");
        assert_eq!(input.read_line().as_deref(), Some("second"));
        assert_eq!(
            input.read_line().as_deref(),
            Some("last"),
            "the final unterminated line is still a line"
        );
        assert_eq!(input.read_line(), None, "then end of file");
        assert_eq!(input.read_line(), None, "and it stays end of file");
    }

    #[test]
    fn read_to_end_drains_from_the_cursor() {
        let mut input = StdinSource::new(b"one\ntwo\nthree".to_vec());
        assert_eq!(input.read_line().as_deref(), Some("one"));
        assert_eq!(
            input.read_to_end(),
            b"two\nthree",
            "read_to_end takes what a line read left behind"
        );
        assert!(input.read_to_end().is_empty(), "and then nothing");
    }
}
