//! Bounded, seeded input generators for the panic-fuzzer. Three strategies, combined: the goal is
//! deep error-state coverage of the front-end, not valid programs.
//!
//! 1. **Random UTF-8-ish bytes** — mostly ASCII printable + occasional high bytes / newlines /
//!    tabs. Raw pass-through: invalid UTF-8 just yields chezzi's clean read error (a non-finding),
//!    so we don't need to keep it valid.
//! 2. **Token-alphabet sampler** — a hard-coded alphabet of Chezzi keyword / punctuation / operator
//!    spellings (mirroring `src/lexer/mod.rs`) plus random identifiers, numbers, newlines and
//!    leading-space indentation, joined by weighted random draws to reach deep parser states. Not
//!    grammar-balanced on purpose.
//! 3. **Structure-aware raw-byte mutation** of a corpus example (`examples/*.chz`): byte flips,
//!    truncation, duplicated / removed lines, inserted braces / colons / indentation. Byte-level,
//!    not AST. If the corpus is empty it falls back to strategy 2.
//!
//! Every generator is bounded to `MAX_LEN` bytes. Same seed ⇒ same input (the unit of reproduction
//! via `panicfuzz --seed N`).

use super::rng::Rng;

/// Hard cap on every generated input (the task budget: "<= ~2KB").
const MAX_LEN: usize = 2048;

/// Chezzi keyword / operator / punctuation spellings, mirroring `src/lexer/mod.rs`. The sampler
/// joins these (plus idents / numbers / layout) into adversarial token soup.
const TOKENS: &[&str] = &[
    // keywords
    "fn", "return", "if", "else", "for", "while", "in", "break", "continue", "struct", "enum",
    "protocol", "type", "newtype", "match", "recover", "defer", "assert", "test", "spawn",
    "parallel", "wait", "yield", "import", "extern", "from", "as", "ref", "and", "or", "not",
    "true", "false", "nil", // operators
    "+", "-", "*", "/", "%", "=", ":=", "==", "!=", "<", "<=", ">", ">=", "+=", "-=", "*=", "/=",
    "%=", "&=", "|=", "^=", "<<=", ">>=", "->", "|>", "?", "?.", "??", "!", "&", "^", "|", "<<",
    ">>", // delimiters
    "(", ")", "[", "]", "{", "}", ",", ":", ".", "..",
    // common identifiers / type names to drive the checker
    "print", "len", "int", "str", "float", "bool", "List", "Map", "Set", "Result", "Option", "self",
    "x", "y", "foo",
];

/// Identifier characters for synthesized names.
const IDENT_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789";

/// Pick the strategy for a seed/corpus, returning the seeded RNG positioned right after the
/// strategy draw (so `gen_input` and `strategy_of` stay in lock-step).
fn choose_strategy(seed: u64, corpus: &[Vec<u8>]) -> (Rng, usize) {
    let mut rng = Rng::seed(seed);
    let mut s = rng.below(3) as usize; // 0 = random bytes, 1 = token sampler, 2 = mutation
    if s == 2 && corpus.is_empty() {
        s = 1; // mutation needs a corpus; degrade to the token sampler
    }
    (rng, s)
}

/// Which strategy (0/1/2) seed `seed` will use against `corpus`. Exposed for the coverage test.
pub fn strategy_of(seed: u64, corpus: &[Vec<u8>]) -> usize {
    choose_strategy(seed, corpus).1
}

/// Generate one bounded candidate input for `seed`. Deterministic in `(seed, corpus)`.
pub fn gen_input(seed: u64, corpus: &[Vec<u8>]) -> Vec<u8> {
    let (mut rng, s) = choose_strategy(seed, corpus);
    let mut out = match s {
        0 => random_bytes(&mut rng),
        1 => token_sampler(&mut rng),
        _ => mutate_corpus(&mut rng, corpus),
    };
    out.truncate(MAX_LEN);
    out
}

/// Strategy 1: random UTF-8-ish bytes. Mostly ASCII printable, with occasional high bytes / control
/// / layout so the lexer's read + indentation paths get hit too.
fn random_bytes(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(MAX_LEN as u64) as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let r = rng.below(100);
        let b = if r < 80 {
            // printable ASCII 0x20..=0x7e
            0x20 + rng.below(0x7f - 0x20) as u8
        } else if r < 88 {
            b'\n'
        } else if r < 92 {
            b'\t'
        } else if r < 95 {
            b' '
        } else {
            // a high / non-UTF-8-ish byte
            0x80 + rng.below(0x80) as u8
        };
        out.push(b);
    }
    out
}

/// Strategy 2: token-alphabet soup with idents, numbers, newlines and indentation.
fn token_sampler(rng: &mut Rng) -> Vec<u8> {
    let pieces = 1 + rng.below(200) as usize;
    let mut out: Vec<u8> = Vec::new();
    for _ in 0..pieces {
        if out.len() >= MAX_LEN {
            break;
        }
        let r = rng.below(100);
        if r < 55 {
            out.extend_from_slice(rng.choice(TOKENS).as_bytes());
            out.push(b' ');
        } else if r < 70 {
            // identifier
            let n = 1 + rng.below(8) as usize;
            for _ in 0..n {
                out.push(*rng.choice(IDENT_CHARS));
            }
            out.push(b' ');
        } else if r < 82 {
            // number (decimal / radix-prefixed)
            let n = 1 + rng.below(12) as usize;
            for _ in 0..n {
                out.push(b'0' + rng.below(10) as u8);
            }
            if rng.chance(0.3) {
                out.push(b'.');
                out.push(b'0' + rng.below(10) as u8);
            }
            out.push(b' ');
        } else if r < 92 {
            out.push(b'\n');
        } else {
            // leading indentation on a fresh line
            out.push(b'\n');
            let spaces = rng.below(12) as usize;
            out.extend(std::iter::repeat_n(b' ', spaces));
        }
    }
    out
}

/// Strategy 3: structure-aware raw-byte mutation of a corpus example.
fn mutate_corpus(rng: &mut Rng, corpus: &[Vec<u8>]) -> Vec<u8> {
    if corpus.is_empty() {
        return token_sampler(rng); // defensive: choose_strategy already guards this
    }
    let mut buf = rng.choice(corpus).clone();
    let rounds = 1 + rng.below(8) as usize;
    for _ in 0..rounds {
        if buf.is_empty() {
            buf.push(b'\n');
        }
        match rng.below(7) {
            0 => {
                // byte flip
                let i = rng.pick(buf.len());
                buf[i] ^= 1 << rng.below(8);
            }
            1 => {
                // truncate at a random point
                let i = rng.pick(buf.len());
                buf.truncate(i);
            }
            2 => {
                // duplicate a line
                if let Some(line) = nth_line(&buf, rng) {
                    let at = rng.pick(buf.len());
                    splice(&mut buf, at, &line);
                }
            }
            3 => {
                // remove a line
                remove_random_line(&mut buf, rng);
            }
            4 => {
                // insert an unbalanced brace / bracket / paren
                let c = b"{}[]()"[rng.pick(6)];
                let at = rng.pick(buf.len());
                buf.insert(at, c);
            }
            5 => {
                // insert a colon
                let at = rng.pick(buf.len());
                buf.insert(at, b':');
            }
            _ => {
                // insert indentation (a newline + spaces/tab) to perturb layout
                let at = rng.pick(buf.len());
                let mut ins = vec![b'\n'];
                let spaces = rng.below(9) as usize;
                ins.extend(std::iter::repeat_n(b' ', spaces));
                if rng.chance(0.3) {
                    ins.push(b'\t');
                }
                splice(&mut buf, at, &ins);
            }
        }
        if buf.len() > MAX_LEN {
            buf.truncate(MAX_LEN);
        }
    }
    buf
}

/// Borrow a random `\n`-terminated line (including its newline) from `buf`.
fn nth_line(buf: &[u8], rng: &mut Rng) -> Option<Vec<u8>> {
    let lines: Vec<&[u8]> = split_keep_newline(buf);
    if lines.is_empty() {
        return None;
    }
    Some(lines[rng.pick(lines.len())].to_vec())
}

fn remove_random_line(buf: &mut Vec<u8>, rng: &mut Rng) {
    let lines: Vec<&[u8]> = split_keep_newline(buf);
    if lines.len() < 2 {
        return;
    }
    let drop = rng.pick(lines.len());
    let mut out = Vec::with_capacity(buf.len());
    for (i, l) in lines.iter().enumerate() {
        if i != drop {
            out.extend_from_slice(l);
        }
    }
    *buf = out;
}

/// Split into lines, each retaining its trailing `\n` (the final line may lack one).
fn split_keep_newline(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            out.push(&buf[start..=i]);
            start = i + 1;
        }
    }
    if start < buf.len() {
        out.push(&buf[start..]);
    }
    out
}

fn splice(buf: &mut Vec<u8>, at: usize, ins: &[u8]) {
    let at = at.min(buf.len());
    let tail = buf.split_off(at);
    buf.extend_from_slice(ins);
    buf.extend_from_slice(&tail);
}
