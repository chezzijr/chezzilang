//! `std.encoding` — reversible text codecs (base64, hex, URL percent-encoding).
//!
//! The `str` members take a `str` and operate on its UTF-8 bytes (matching `bytes(s)` / `s.encode()`),
//! returning `str` (infallible encode) or `Result[str]` (decode — malformed input or non-UTF-8
//! decoded bytes are a recoverable `Err`, never a panic): a str-typed decode that would produce
//! arbitrary bytes is UTF-8-validated and surfaced as an `Err`. R1 added the BINARY twins
//! `base64_encode_bytes` / `base64_decode_bytes`, which carry raw `bytes` across the native seam
//! ([`super::Host::arg_bytes`] / [`super::NativeRet::Bytes`]) and so round-trip arbitrary data.
//!
//! - base64: RFC 4648 — `base64_encode`/`base64_decode` (std `+/` alphabet, `=` padding), the
//!   URL-safe `base64_encode_url`/`base64_decode_url` (`-_` alphabet), and the binary
//!   `base64_encode_bytes`/`base64_decode_bytes` (std alphabet). Decode strips/accepts padding;
//!   the std decoder rejects `-_` and the url decoder rejects `+/`.
//! - hex: lowercase `hex_encode`, `hex_decode` (rejects odd length / non-hex digits).
//! - url: RFC 3986 COMPONENT percent-encoding — `url_encode` keeps the unreserved set
//!   `A-Za-z0-9-._~` literal and `%XX`-escapes everything else (uppercase hex); `url_decode` reverses
//!   it (strict — `+` is NOT treated as space; that is `application/x-www-form-urlencoded`, not 3986).
//!   `query_encode(map[str,str])` builds a `k=v&…` query string (both sides percent-encoded, keys
//!   sorted by raw value for determinism, empty map → "").
//!
//! All members are pure CPU str transforms (no I/O), so none are in [`super::is_blocking`] — they run
//! inline on every engine, giving 3-engine parity by construction at the NativeFn seam.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};

const STD_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode `bytes` to base64 using `alphabet`, with `=` padding.
fn b64_encode(bytes: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n >> 18) & 0x3F) as usize] as char);
        out.push(alphabet[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode a base64 `str` to raw bytes using `alphabet`. Rejects out-of-alphabet chars, bad length,
/// and misplaced padding. Padding is required to a multiple of 4 (canonical RFC 4648).
fn b64_decode(s: &str, alphabet: &[u8; 64]) -> Result<Vec<u8>, HostError> {
    let mut rev = [255u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(HostError {
            message: "base64: invalid length (not a multiple of 4)".into(),
        });
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let quad = &bytes[i..i + 4];
        let mut vals = [0u32; 4];
        let mut pad = 0;
        for (j, &c) in quad.iter().enumerate() {
            if c == b'=' {
                // Padding only valid in the last two positions of the final quad.
                if i + 4 != bytes.len() || j < 2 {
                    return Err(HostError {
                        message: "base64: misplaced padding".into(),
                    });
                }
                pad += 1;
                vals[j] = 0;
            } else {
                if pad > 0 {
                    return Err(HostError {
                        message: "base64: data after padding".into(),
                    });
                }
                let v = rev[c as usize];
                if v == 255 {
                    return Err(HostError {
                        message: format!("base64: invalid character {:?}", c as char),
                    });
                }
                vals[j] = v as u32;
            }
        }
        let n = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
        i += 4;
    }
    Ok(out)
}

/// Lower decoded bytes to a `Result[str]`: valid UTF-8 → `Ok`, else a recoverable `Err`.
fn bytes_to_result(bytes: Vec<u8>, codec: &str) -> NativeRet {
    match String::from_utf8(bytes) {
        Ok(s) => NativeRet::Ok(Box::new(NativeRet::Str(s))),
        Err(_) => NativeRet::Err(format!("{codec}: decoded bytes are not valid UTF-8")),
    }
}

fn base64_encode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_encode", 1)?;
    let s = h.arg_str(0)?;
    Ok(NativeRet::Str(b64_encode(s.as_bytes(), STD_ALPHABET)))
}

fn base64_encode_url(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_encode_url", 1)?;
    let s = h.arg_str(0)?;
    Ok(NativeRet::Str(b64_encode(s.as_bytes(), URL_ALPHABET)))
}

fn base64_decode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_decode", 1)?;
    let s = h.arg_str(0)?;
    match b64_decode(&s, STD_ALPHABET) {
        Ok(bytes) => Ok(bytes_to_result(bytes, "base64")),
        Err(e) => Ok(NativeRet::Err(e.message)),
    }
}

fn base64_decode_url(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_decode_url", 1)?;
    let s = h.arg_str(0)?;
    match b64_decode(&s, URL_ALPHABET) {
        Ok(bytes) => Ok(bytes_to_result(bytes, "base64")),
        Err(e) => Ok(NativeRet::Err(e.message)),
    }
}

/// R1 — the BINARY twins of `base64_encode`/`base64_decode` (std alphabet): they take/return raw
/// `bytes` instead of round-tripping through UTF-8, so arbitrary binary data survives.
fn base64_encode_bytes(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_encode_bytes", 1)?;
    let b = h.arg_bytes(0)?;
    Ok(NativeRet::Str(b64_encode(&b, STD_ALPHABET)))
}

fn base64_decode_bytes(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "base64_decode_bytes", 1)?;
    let s = h.arg_str(0)?;
    match b64_decode(&s, STD_ALPHABET) {
        Ok(bytes) => Ok(NativeRet::Ok(Box::new(NativeRet::Bytes(bytes)))),
        Err(e) => Ok(NativeRet::Err(e.message)),
    }
}

fn hex_encode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "hex_encode", 1)?;
    let s = h.arg_str(0)?;
    Ok(NativeRet::Str(hex_encode_bytes(s.as_bytes())))
}

/// Encode bytes to lowercase hex.
fn hex_encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xF) as u32, 16).unwrap());
    }
    out
}

fn hex_decode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "hex_decode", 1)?;
    let s = h.arg_str(0)?;
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Ok(NativeRet::Err("hex: odd-length input".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return Ok(NativeRet::Err("hex: invalid hex digit".into())),
        }
        i += 2;
    }
    Ok(bytes_to_result(out, "hex"))
}

/// RFC 3986 unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~".
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// RFC 3986 COMPONENT percent-encoding of `s`: unreserved bytes stay literal, everything else
/// becomes `%XX` (uppercase hex). The single shared percent-encoder reused by `url_encode` and
/// `query_encode` (no duplicated escaper).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 0xF) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

fn url_encode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "url_encode", 1)?;
    let s = h.arg_str(0)?;
    Ok(NativeRet::Str(percent_encode(&s)))
}

/// `query_encode(params: map[str, str]) -> str` — assemble a `k=v&k2=v2` query string. Both key and
/// value are percent-encoded (reusing [`percent_encode`], the same escaper as `url_encode`), pairs
/// joined with `&` and key/value with `=`. Keys are SORTED by their RAW (pre-encoding) bytes so the
/// output is deterministic regardless of map iteration order — giving a stable golden and 3-engine
/// parity by construction. An empty map yields `""` (no leading `?`); the caller composes
/// `url + "?" + query_encode(params)`.
fn query_encode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "query_encode", 1)?;
    let mut pairs = h.arg_str_map(0)?;
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&percent_encode(k));
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    Ok(NativeRet::Str(out))
}

fn url_decode(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "url_decode", 1)?;
    let s = h.arg_str(0)?;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Ok(NativeRet::Err("url: truncated percent-escape".into()));
            }
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                _ => return Ok(NativeRet::Err("url: invalid percent-escape".into())),
            }
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    bytes_to_result_url(out)
}

/// url_decode lowering (same UTF-8 validation, distinct codec label).
fn bytes_to_result_url(bytes: Vec<u8>) -> Result<NativeRet, HostError> {
    Ok(bytes_to_result(bytes, "url"))
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("base64_encode", base64_encode),
    ("base64_encode_url", base64_encode_url),
    ("base64_decode", base64_decode),
    ("base64_decode_url", base64_decode_url),
    ("base64_encode_bytes", base64_encode_bytes),
    ("base64_decode_bytes", base64_decode_bytes),
    ("hex_encode", hex_encode),
    ("hex_decode", hex_decode),
    ("url_encode", url_encode),
    ("url_decode", url_decode),
    ("query_encode", query_encode),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal str-only `Host` for exercising the encoding natives in isolation. Carries one str
    /// slot per arg; the encoding fns read `arg_str(0)`.
    #[derive(Default)]
    struct StrHost {
        strs: Vec<String>,
        map: Vec<(String, String)>,
    }

    impl Host for StrHost {
        fn arg_count(&self) -> usize {
            self.strs.len()
        }
        fn arg_int(&mut self, _i: usize) -> Result<i64, HostError> {
            Err(HostError {
                message: "no int args".into(),
            })
        }
        fn arg_float(&mut self, _i: usize) -> Result<f64, HostError> {
            Err(HostError {
                message: "no float args".into(),
            })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
        }
        fn arg_str(&mut self, i: usize) -> Result<String, HostError> {
            self.strs.get(i).cloned().ok_or(HostError {
                message: "missing arg".into(),
            })
        }
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Ok(self.map.clone())
        }
        fn write_stdout(&mut self, _s: &str) {}
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(None)
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<String, HostError> {
            Ok("/".into())
        }
    }

    fn host(s: &str) -> StrHost {
        StrHost {
            strs: vec![s.to_string()],
            map: vec![],
        }
    }

    fn enc(f: NativeFn, s: &str) -> String {
        match f(&mut host(s)).unwrap() {
            NativeRet::Str(out) => out,
            other => panic!("expected Str, got {other:?}"),
        }
    }

    fn dec_ok(f: NativeFn, s: &str) -> String {
        match f(&mut host(s)).unwrap() {
            NativeRet::Ok(b) => match *b {
                NativeRet::Str(out) => out,
                other => panic!("expected Ok(Str), got {other:?}"),
            },
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    fn dec_err(f: NativeFn, s: &str) {
        match f(&mut host(s)).unwrap() {
            NativeRet::Err(_) => {}
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// RFC 4648 §10 base64 test vectors (std alphabet).
    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(enc(base64_encode, ""), "");
        assert_eq!(enc(base64_encode, "f"), "Zg==");
        assert_eq!(enc(base64_encode, "fo"), "Zm8=");
        assert_eq!(enc(base64_encode, "foo"), "Zm9v");
        assert_eq!(enc(base64_encode, "foob"), "Zm9vYg==");
        assert_eq!(enc(base64_encode, "fooba"), "Zm9vYmE=");
        assert_eq!(enc(base64_encode, "foobar"), "Zm9vYmFy");
        assert_eq!(enc(base64_encode, "Man"), "TWFu");
    }

    #[test]
    fn base64_roundtrip_and_decode_errors() {
        assert_eq!(dec_ok(base64_decode, "TWFu"), "Man");
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let e = enc(base64_encode, s);
            assert_eq!(dec_ok(base64_decode, &e), s);
        }
        // Malformed input → recoverable Err (no panic).
        dec_err(base64_decode, "!!!!");
        dec_err(base64_decode, "abc"); // length not a multiple of 4
        dec_err(base64_decode, "ab=c"); // misplaced padding
        // base64 of a single 0xFF byte ("/w==") decodes to a non-UTF-8 byte → Err.
        let enc_ff = b64_encode(&[0xFF], STD_ALPHABET);
        dec_err(base64_decode, &enc_ff);
    }

    #[test]
    fn base64_url_safe_variant() {
        // 0xFB, 0xFF produces "+/" in std; "-_" in url-safe.
        let bytes = [0xFBu8, 0xFF];
        assert_eq!(b64_encode(&bytes, STD_ALPHABET), "+/8=");
        assert_eq!(b64_encode(&bytes, URL_ALPHABET), "-_8=");
        // url round-trips on text.
        assert_eq!(
            dec_ok(base64_decode_url, &enc(base64_encode_url, "foobar")),
            "foobar"
        );
        // url decoder rejects a std-alphabet "+/" input.
        dec_err(base64_decode_url, "+/8=");
        // std decoder rejects a url-alphabet "-_" input.
        dec_err(base64_decode, "-_8=");
    }

    #[test]
    fn hex_vectors_and_errors() {
        assert_eq!(enc(hex_encode, ""), "");
        assert_eq!(enc(hex_encode, "foo"), "666f6f");
        assert_eq!(dec_ok(hex_decode, "666f6f"), "foo");
        assert_eq!(dec_ok(hex_decode, ""), "");
        for s in ["", "f", "foo", "foobar"] {
            assert_eq!(dec_ok(hex_decode, &enc(hex_encode, s)), s);
        }
        dec_err(hex_decode, "zz"); // non-hex digit
        dec_err(hex_decode, "abc"); // odd length
        dec_err(hex_decode, &hex_encode_bytes(&[0xFF])); // decodes to non-UTF-8
    }

    #[test]
    fn url_encode_rfc3986_component() {
        assert_eq!(enc(url_encode, "a b/c?d"), "a%20b%2Fc%3Fd");
        // unreserved set stays literal.
        assert_eq!(enc(url_encode, "a~b-c_d.e"), "a~b-c_d.e");
        assert_eq!(dec_ok(url_decode, "a%20b%2Fc%3Fd"), "a b/c?d");
        for s in ["", "hello world", "a/b?c=d&e", "résumé"] {
            assert_eq!(dec_ok(url_decode, &enc(url_encode, s)), s);
        }
        dec_err(url_decode, "%2G"); // bad hex
        dec_err(url_decode, "%2"); // truncated
        // %FF decodes to a lone 0xFF byte → non-UTF-8 → Err.
        dec_err(url_decode, "%FF");
    }

    /// Build a map-carrying host and run `query_encode`, returning the assembled query string.
    fn qe(pairs: &[(&str, &str)]) -> String {
        // arg 0 is the map; a single str slot makes `arg_count() == 1` so `expect_args` passes
        // (the placeholder str is never read — `query_encode` reads `arg_str_map(0)`).
        let mut h = StrHost {
            strs: vec![String::new()],
            map: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        match query_encode(&mut h).unwrap() {
            NativeRet::Str(out) => out,
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn query_encode_sorts_and_encodes_both_sides() {
        // Keys sorted by RAW key (a&b < q < x); both key and value percent-encoded with the
        // url_encode contract: space->%20, &->%26, ==->%3D.
        assert_eq!(
            qe(&[("q", "a b"), ("x", "1"), ("a&b", "c=d")]),
            "a%26b=c%3Dd&q=a%20b&x=1"
        );
    }

    #[test]
    fn query_encode_empty_map_is_empty_string() {
        assert_eq!(qe(&[]), "");
    }

    #[test]
    fn query_encode_empty_value() {
        assert_eq!(qe(&[("k", "")]), "k=");
    }
}
