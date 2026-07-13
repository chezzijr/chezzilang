//! `std.io` — native I/O (M6c): line output, stdin, whole-string file read/write.
//!
//! File handles / streaming are intentionally out of scope (no userdata this milestone): files are
//! read and written as whole strings, which covers the common scripting case. Errors come back as
//! `Result` values (the engine lowers `NativeRet::Err` to `Err(msg)`), never panics.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::io::Read;

/// Upper bound on `read_file` input, so a huge or unbounded file (`/dev/zero`, a multi-GB log)
/// returns a clean error instead of exhausting memory — mirroring `range`'s length cap.
const MAX_READ_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn print(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "print", 1)?;
    let s = h.arg_str(0)?;
    // ONE host write (body + newline): under the streaming CLI that is one locked write → the line
    // is atomic across tasks. Byte-identical under the buffered sink.
    h.write_stdout(&format!("{s}\n"));
    Ok(NativeRet::Nil)
}

fn eprint(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "eprint", 1)?;
    let s = h.arg_str(0)?;
    h.write_stderr(&format!("{s}\n"));
    Ok(NativeRet::Nil)
}

fn read_line(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "read_line", 0)?;
    match h.read_line()? {
        Some(line) => Ok(NativeRet::Some(Box::new(NativeRet::Str(line)))),
        None => Ok(NativeRet::None),
    }
}

/// Flush this process's stdout (the streaming CLI buffers by line — `Stdout` is a `LineWriter`). A
/// no-op under the buffered/captured sink (tests, embedders), where nothing is going anywhere yet.
fn flush(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "flush", 0)?;
    h.flush_stdout();
    Ok(NativeRet::Nil)
}

/// `input(prompt)` — write the prompt with NO trailing newline, flush, then read one line. Returns
/// exactly what `read_line` returns: `Some(line)` (newline stripped), `None` at EOF.
fn input(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "input", 1)?;
    let prompt = h.arg_str(0)?;
    h.write_stdout(&prompt);
    h.flush_stdout();
    match h.read_line()? {
        Some(line) => Ok(NativeRet::Some(Box::new(NativeRet::Str(line)))),
        None => Ok(NativeRet::None),
    }
}

fn read_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "read_file", 1)?;
    let path = h.arg_str(0)?;
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => return Ok(NativeRet::Err(format!("{path}: {e}"))),
    };
    // Read at most the cap + 1 byte: if we got more than the cap, the file is over-limit.
    let mut buf = String::new();
    match file.take(MAX_READ_FILE_BYTES + 1).read_to_string(&mut buf) {
        Ok(_) if buf.len() as u64 > MAX_READ_FILE_BYTES => Ok(NativeRet::Err(format!(
            "{path}: file exceeds the {MAX_READ_FILE_BYTES}-byte read limit"
        ))),
        Ok(_) => Ok(NativeRet::Ok(Box::new(NativeRet::Str(buf)))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

fn write_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "write_file", 2)?;
    let path = h.arg_str(0)?;
    let contents = h.arg_str(1)?;
    match std::fs::write(&path, &contents) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("print", print),
    ("eprint", eprint),
    ("read_line", read_line),
    ("flush", flush),
    ("input", input),
    ("read_file", read_file),
    ("write_file", write_file),
];
