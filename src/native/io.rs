//! `std.io` — native I/O (M6c): line output, stdin, whole-string file read/write.
//!
//! File handles / streaming are intentionally out of scope (no userdata this milestone): files are
//! read and written as whole strings, which covers the common scripting case. Errors come back as
//! `Result` values (the engine lowers `NativeRet::Err` to `Err(msg)`), never panics.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};
use std::io::Read;

/// Upper bound on `read_file` input, so a huge or unbounded file (`/dev/zero`, a multi-GB log)
/// returns a clean error instead of exhausting memory — mirroring `range`'s length cap.
const MAX_READ_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn print(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "print", 1)?;
    let s = h.arg_str(0)?;
    h.write_stdout(&s);
    h.write_stdout("\n");
    Ok(NativeRet::Nil)
}

fn eprint(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "eprint", 1)?;
    let s = h.arg_str(0)?;
    h.write_stderr(&s);
    h.write_stderr("\n");
    Ok(NativeRet::Nil)
}

fn read_line(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "read_line", 0)?;
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
        Ok(_) if buf.len() as u64 > MAX_READ_FILE_BYTES => {
            Ok(NativeRet::Err(format!("{path}: file exceeds the {MAX_READ_FILE_BYTES}-byte read limit")))
        }
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
    ("read_file", read_file),
    ("write_file", write_file),
];
