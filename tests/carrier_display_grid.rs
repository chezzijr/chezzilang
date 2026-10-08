//! TICKET-228: a carrier prints as the user writes it. Carrier {`T?`, `T!E`, `None!E`, `T??`,
//! `T?!E`} x position {print, interpolation, `str()`, list element, map value, fault text,
//! `recover:` text}. A present or successful value prints as its payload, an error as `!` then
//! the error, an absent value as `None`. `?` prints only in front of an absent payload, so a
//! present-but-absent `T??` (`?None`) stays distinct from `None`. One generated program per cell,
//! run through the built `chezzi` binary.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LIB: &str = "fn save(n: int) -> None!str:
    if n > 0:
        return
    return !\"full\"
x: int? = 5
n: int? = None
s: str? = \"hi\"
r: int!str = 5
e: int!str = !\"boom\"
some_none: int?? = ?n
some_some: int?? = ?x
deep_ok: int?!str = ?x
deep_none: int?!str = ?n
deep_err: int?!str = !\"deep\"
";

fn cell(name: &str, body: &str, expect: Expect) -> Cell {
    Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), format!("{LIB}{body}\n"))],
        expect,
    }
}

fn prints(name: &str, body: &str, want: &str) -> Cell {
    cell(name, body, Expect::Prints(want.to_string()))
}

#[test]
fn carrier_display_grid() {
    let cells = vec![
        // ---- T?
        prints("T? present print", "print(x)", "5"),
        prints("T? present interpolation", "print(\"<{x}>\")", "<5>"),
        prints("T? present str()", "print(str(x) + \".\")", "5."),
        prints("T? present list", "print([x])", "[5]"),
        prints("T? absent print", "print(n)", "None"),
        prints("T? absent interpolation", "print(\"<{n}>\")", "<None>"),
        prints("T? absent list", "print([n, x])", "[None, 5]"),
        prints("T? str payload print", "print(s)", "hi"),
        prints("T? str payload list", "print([s])", "['hi']"),
        prints("T? map value", "print({\"k\": s})", "{'k': 'hi'}"),
        // ---- T!E
        prints("T!E success print", "print(r)", "5"),
        prints("T!E success list", "print([r])", "[5]"),
        prints("T!E error print", "print(e)", "!boom"),
        prints("T!E error interpolation", "print(\"<{e}>\")", "<!boom>"),
        prints("T!E error str()", "print(str(e) + \".\")", "!boom."),
        prints("T!E error list", "print([e, r])", "[!'boom', 5]"),
        prints("T!E error map value", "print({\"k\": e})", "{'k': !'boom'}"),
        // ---- None!E
        prints("None!E success print", "print(save(1))", "None"),
        prints("None!E error print", "print(save(0))", "!full"),
        prints(
            "None!E list",
            "print([save(1), save(0)])",
            "[None, !'full']",
        ),
        // ---- T??: `?` prints only in front of an absent payload.
        prints("T?? present absent", "print(some_none)", "?None"),
        prints("T?? present present", "print(some_some)", "5"),
        prints(
            "T?? list",
            "outer: int?? = None\nprint([some_none, some_some, outer])",
            "[?None, 5, None]",
        ),
        // ---- T?!E
        prints("T?!E success present", "print(deep_ok)", "5"),
        prints("T?!E success absent", "print(deep_none)", "None"),
        prints("T?!E error", "print(deep_err)", "!deep"),
        // ---- recover: text and fault text.
        prints(
            "recover text",
            "fn boom() -> int:\n    panic(\"p\")\nres := recover: boom()\nprint(res)\nprint([res])",
            "!p\n[!'p']",
        ),
        cell(
            "fault text of a failed assert",
            "assert x == None",
            Expect::Rejects("assertion failed: 5 == None"),
        ),
        cell(
            "fault text of a failed assert on an error",
            "assert e == r",
            Expect::Rejects("assertion failed: !'boom' == 5"),
        ),
    ];
    run_grid("carrier-display", &cells);
}
