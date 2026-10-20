//! One `#[test]` per row of a grid, so nextest runs the rows in parallel and gives each its own
//! time limit (TICKET-276). Include it with
//! `#[path = "support/grid_tests.rs"] #[macro_use] mod grid_tests;`.
//!
//! `grid_tests!(LIST in module: [a, b] => crate::run)` defines `const LIST: &[&str]` and one test
//! `module::a` per row that calls `run("a")`. `grid_tests!(OUTER x INNER: [a, b] x [p, q] => crate::run)`
//! defines both lists and one test `a::p` per pair that calls `run("a", "p")`. The three-list form
//! `A x B x C: [..] x [..] x [..]` defines one test `a::p::x` per triple that calls `run("a", "p", "x")`.
//! A row that is a Rust keyword is written as a raw identifier (`r#struct`); its name in the list is
//! `struct`. The run function is named from the crate root (`crate::run`), so every depth reaches it.

#![allow(dead_code)]

/// A row's name: its identifier with a raw identifier's `r#` removed.
pub const fn bare(ident: &'static str) -> &'static str {
    match ident.as_bytes() {
        [b'r', b'#', rest @ ..] => match core::str::from_utf8(rest) {
            Ok(name) => name,
            Err(_) => panic!("a row name is ASCII"),
        },
        _ => ident,
    }
}

macro_rules! grid_tests {
    ($list:ident in $module:ident: [$($row:ident),+ $(,)?] => $run:path) => {
        #[allow(dead_code)]
        const $list: &[&str] = &[$($crate::grid_tests::bare(stringify!($row))),+];
        mod $module {
            $(
                #[test]
                fn $row() {
                    $run($crate::grid_tests::bare(stringify!($row)));
                }
            )+
        }
    };
    ($outer:ident x $mid:ident x $inner:ident: [$($o:ident),+ $(,)?] x $mids:tt x $inners:tt => $run:path) => {
        #[allow(dead_code)]
        const $outer: &[&str] = &[$($crate::grid_tests::bare(stringify!($o))),+];
        #[allow(dead_code)]
        const $mid: &[&str] = &grid_tests!(@names $mids);
        #[allow(dead_code)]
        const $inner: &[&str] = &grid_tests!(@names $inners);
        $(grid_tests!(@outer3 $o $mids $inners $run);)+
    };
    ($outer:ident x $inner:ident: [$($o:ident),+ $(,)?] x $rows:tt => $run:path) => {
        #[allow(dead_code)]
        const $outer: &[&str] = &[$($crate::grid_tests::bare(stringify!($o))),+];
        #[allow(dead_code)]
        const $inner: &[&str] = &grid_tests!(@names $rows);
        $(grid_tests!(@module $o $rows $run);)+
    };
    (@names [$($i:ident),+ $(,)?]) => {
        [$($crate::grid_tests::bare(stringify!($i))),+]
    };
    (@module $o:ident [$($i:ident),+ $(,)?] $run:path) => {
        mod $o {
            $(
                #[test]
                fn $i() {
                    $run(
                        $crate::grid_tests::bare(stringify!($o)),
                        $crate::grid_tests::bare(stringify!($i)),
                    );
                }
            )+
        }
    };
    (@outer3 $o:ident [$($m:ident),+ $(,)?] $inners:tt $run:path) => {
        mod $o {
            $(grid_tests!(@mid3 $o $m $inners $run);)+
        }
    };
    (@mid3 $o:ident $m:ident [$($i:ident),+ $(,)?] $run:path) => {
        mod $m {
            $(
                #[test]
                fn $i() {
                    $run(
                        $crate::grid_tests::bare(stringify!($o)),
                        $crate::grid_tests::bare(stringify!($m)),
                        $crate::grid_tests::bare(stringify!($i)),
                    );
                }
            )+
        }
    };
}
