//! A lenient HTTP/1 response-head reader for `std.request` (W14-30c).
//!
//! ureq 3 parses response heads with `httparse`, which refuses an `HTTP/1.2` status line and an
//! obs-fold continuation that BOTH ancestors accept (MEASURED 2026-09-20, Go 1.27.0 and CPython
//! 3.14.7). `ureq-proto` drives `httparse` with its default config and exposes no leniency knob,
//! and DEC-143 declined the `http`-crate + `catch_unwind` route, so the repair sits one layer
//! BELOW the parser: this connector wraps the transport and rewrites the head on the wire.

use std::io;

use ureq::Error;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};

/// Whether `b` is a byte `httparse` accepts inside a header value (`HEADER_VALUE_MAP` in
/// `httparse-1.10.1/src/lib.rs:90` is `\t | 0x20..=0x7e | 0x80..=0xff`). This must never be
/// STRICTER than httparse's, or the wrapper would newly refuse a response ureq accepts today.
fn valid_value_byte(b: u8) -> bool {
    b == b'\t' || (0x20..=0x7e).contains(&b) || b >= 0x80
}

/// Render a head line the way Go's `malformed MIME header line:` does: printable ASCII verbatim,
/// everything else as `\xNN`.
fn quote_line(line: &[u8]) -> String {
    let mut s = String::with_capacity(line.len() + 2);
    s.push('"');
    for &b in line {
        if (0x20..=0x7e).contains(&b) && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            s.push_str(&format!("\\x{b:02x}"));
        }
    }
    s.push('"');
    s
}

/// The end of the response head (index just past the blank line), accepting CRLF and bare LF.
fn head_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some(i + 4);
        }
        if buf[i..].starts_with(b"\n\n") {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Rewrite one response head so `httparse` accepts what Go and CPython accept.
///
/// * An obs-fold continuation line (leading SP/HTAB) joins onto the previous header line with one
///   space, which is what Go's `textproto` yields (`X-Fold: a\r\n b` -> `a b`, MEASURED).
/// * `HTTP/1.<d>` with `<d>` neither `0` nor `1` becomes `HTTP/1.1`. Only a single-digit 1.x
///   minor: `HTTP/2.0` and a garbage status line still reach ureq's parser untouched.
/// * A header value carrying a byte `httparse` refuses is an `Err` naming the line, as Go does.
///
/// Everything else is left byte-for-byte alone, so every shape ureq accepts or refuses today keeps
/// its current verdict.
fn sanitize_head(head: &[u8]) -> Result<Vec<u8>, String> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for raw in head.split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        let folded = matches!(line.first(), Some(b' ' | b'\t'));
        // A fold line with no header line before it is RFC-invalid and Go errors on it; leave it
        // for ureq to refuse rather than splicing it onto the status line.
        if folded && lines.len() > 1 {
            let cont = line
                .iter()
                .position(|&b| b != b' ' && b != b'\t')
                .map(|p| &line[p..])
                .unwrap_or(&[]);
            let prev = lines.last_mut().expect("lines is non-empty");
            while matches!(prev.last(), Some(b' ' | b'\t')) {
                prev.pop();
            }
            prev.push(b' ');
            prev.extend_from_slice(cont);
        } else {
            lines.push(line.to_vec());
        }
    }
    if lines.is_empty() {
        return Ok(head.to_vec());
    }
    if lines[0].len() >= 8 && lines[0].starts_with(b"HTTP/1.") {
        let d = lines[0][7];
        if d.is_ascii_digit() && d != b'0' && d != b'1' {
            lines[0][7] = b'1';
        }
    }
    let mut out = Vec::with_capacity(head.len() + 2);
    for (i, line) in lines.iter().enumerate() {
        if i > 0
            && let Some(colon) = line.iter().position(|&b| b == b':')
            && line[colon + 1..].iter().any(|&b| !valid_value_byte(b))
        {
            return Err(format!("malformed MIME header line: {}", quote_line(line)));
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

/// Connector that wraps the transport below it in [`LenientHead`].
#[derive(Debug)]
pub struct LenientHeadConnector;

impl<In: Transport> Connector<In> for LenientHeadConnector {
    type Out = LenientHead<In>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        Ok(chained.map(|inner| LenientHead {
            inner,
            bufs: LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            ),
            cap: details.config.input_buffer_size(),
            expect_head: true,
            head: Vec::new(),
            pending: Vec::new(),
        }))
    }
}

/// A transport that rewrites each response head before ureq's parser sees it.
///
/// It owns the buffers ureq reads and writes, and copies through to the transport below. Head and
/// body are told apart WITHOUT parsing the body's framing: ureq never pipelines, so the bytes that
/// follow any request write are a response head, and everything after that head is body until the
/// next request write. A 1xx head keeps the next read in head mode, so `Expect: 100-continue` and
/// other interim responses are sanitized too. Cost: one extra copy of every body byte.
#[derive(Debug)]
pub struct LenientHead<In> {
    inner: In,
    bufs: LazyBuffers,
    cap: usize,
    expect_head: bool,
    head: Vec<u8>,
    pending: Vec<u8>,
}

impl<In: Transport> LenientHead<In> {
    /// Move up to `bufs`' free space out of `pending`. Returns the bytes handed to ureq.
    fn drain_pending(&mut self) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let n = {
            let dst = self.bufs.input_append_buf();
            let n = dst.len().min(self.pending.len());
            dst[..n].copy_from_slice(&self.pending[..n]);
            n
        };
        self.bufs.input_appended(n);
        self.pending.drain(..n);
        n
    }

    /// Pull whatever the transport below has into `pending`.
    fn pull(&mut self, timeout: NextTimeout) -> Result<(bool, usize), Error> {
        let progress = self.inner.await_input(timeout)?;
        let n = {
            let src = self.inner.buffers().input();
            self.pending.extend_from_slice(src);
            src.len()
        };
        if n > 0 {
            self.inner.buffers().input_consume(n);
        }
        Ok((progress, n))
    }
}

impl<In: Transport> Transport for LenientHead<In> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.bufs
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        // A write means a request is going out, so the next bytes in are a response head.
        self.expect_head = true;
        let ours = self.bufs.output().len();
        let theirs = self.inner.buffers().output().len();
        if ours < amount || theirs < amount {
            // Equal by construction today: both buffers are sized from the same `details.config`.
            // The guard is here because the copy below would otherwise PANIC on a mismatch, and a
            // panic on a transport thread inside a native call aborts the process where `recover:`
            // cannot reach it. Never truncate to the shorter length: a short write sends a
            // truncated request, which is worse than a clean failure.
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "lenient head transport: cannot transmit {amount} bytes (ours {ours}, inner {theirs})"
                ),
            )));
        }
        {
            let src = &self.bufs.output()[..amount];
            let dst = self.inner.buffers().output();
            dst[..amount].copy_from_slice(src);
        }
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        if self.drain_pending() > 0 {
            return Ok(true);
        }
        if !self.expect_head {
            let (progress, n) = self.pull(timeout)?;
            return Ok(self.drain_pending() > 0 || (progress && n > 0));
        }
        loop {
            let (progress, n) = self.pull(timeout)?;
            let mut got = std::mem::take(&mut self.pending);
            self.head.append(&mut got);
            if let Some(end) = head_end(&self.head) {
                let rest = self.head.split_off(end);
                let sanitized = sanitize_head(&self.head)
                    .map_err(|m| Error::Io(io::Error::new(io::ErrorKind::InvalidData, m)))?;
                self.expect_head = sanitized.starts_with(b"HTTP/1.1 1");
                self.pending = sanitized;
                self.pending.extend_from_slice(&rest);
                self.head.clear();
                return Ok(self.drain_pending() > 0);
            }
            if self.head.len() > self.cap {
                // Too big to buffer: hand it over raw and let ureq's parser judge it, exactly as
                // it would without this wrapper.
                self.expect_head = false;
                self.pending = std::mem::take(&mut self.head);
                return Ok(self.drain_pending() > 0);
            }
            if !progress && n == 0 {
                return Ok(false);
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::Timeout;
    use ureq::unversioned::transport::time::Duration;

    /// A transport whose output buffer is smaller than ours. ureq sizes both from the same
    /// `details.config`, so this mismatch is only reachable through a fake transport.
    #[derive(Debug)]
    struct TinyOutput(LazyBuffers);

    impl Transport for TinyOutput {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.0
        }

        fn transmit_output(&mut self, _amount: usize, _timeout: NextTimeout) -> Result<(), Error> {
            Ok(())
        }

        fn await_input(&mut self, _timeout: NextTimeout) -> Result<bool, Error> {
            Ok(false)
        }

        fn is_open(&mut self) -> bool {
            true
        }
    }

    #[test]
    fn a_short_inner_output_buffer_is_an_err_not_a_panic() {
        let mut t = LenientHead {
            inner: TinyOutput(LazyBuffers::new(1024, 8)),
            bufs: LazyBuffers::new(1024, 1024),
            cap: 1024,
            expect_head: false,
            head: Vec::new(),
            pending: Vec::new(),
        };
        let timeout = NextTimeout {
            after: Duration::from_secs(1),
            reason: Timeout::SendRequest,
        };
        let err = t
            .transmit_output(64, timeout)
            .expect_err("a short inner output buffer must be an Err, not a panic");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot transmit 64 bytes") && msg.contains("ours 1024, inner 8"),
            "message must name the amount and both buffer lengths, got {msg}"
        );
    }
}
