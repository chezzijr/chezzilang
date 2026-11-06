//! VM: the one deadlock report: each wait's phrase, the header, and the listing of blocked tasks.
//!
//! Go's model (`fatal error: all goroutines are asleep - deadlock!`, then every goroutine with its
//! wait and stack). A verdict cuts every blocked task; each cut task ends with a deadlock fault
//! whose text is its own wait ([`Wait::phrase`]) at its own site. A join that reduces deadlock
//! slots folds them into one listing in task order ([`fold`]), and an enclosing join folds again,
//! so the run's report lists every blocked task of the run in spawn-tree order. A driver renders it
//! through [`listing`] and [`render`].

use super::{RuntimeError, TraceFrame};
use crate::ast::Span;

/// What one blocked task waits on. THE owner of each wait's wording: the deadlock listing names a
/// task's wait with [`Wait::phrase`], and a wait that cannot block at all faults with
/// [`Wait::fault_text`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wait {
    /// A `recv()` on an empty channel.
    Recv,
    /// A `send()` on a full bounded channel.
    Send,
    /// A `send()` on a rendezvous (`Channel[T](0)`) channel.
    RendezvousSend,
    /// A `wait:` whose arms are all receives on empty channels.
    Select,
    /// A `wait:` with a send arm, none of whose arms can proceed.
    SelectSend,
    /// A `Shared` update guard.
    SharedGuard,
    /// An `RwShared` guard.
    RwSharedGuard,
    /// An `Executor` join (`shutdown()`, `cancel()`).
    ExecutorJoin,
    /// A socket op.
    Socket,
    /// A task of a `parallel:` block whose own wait no site named.
    Nursery,
}

impl Wait {
    /// The wait as the listing names it: `task k: <phrase> at <site>`.
    pub(crate) fn phrase(self) -> &'static str {
        match self {
            Wait::Recv => "recv() on an empty channel",
            Wait::Send => "send() on a full channel",
            Wait::RendezvousSend => "send() on a rendezvous channel",
            Wait::Select => "wait: on channels that are all empty",
            Wait::SelectSend => "wait: on channels none of which can proceed",
            Wait::SharedGuard => "waiting for the Shared guard",
            Wait::RwSharedGuard => "waiting for the RwShared guard",
            Wait::ExecutorJoin => "waiting for an Executor join",
            Wait::Socket => "waiting on a socket",
            Wait::Nursery => "blocked in a parallel: block",
        }
    }

    /// The send wait for a channel of capacity `cap` (`Some(0)` = rendezvous).
    pub(crate) fn send(cap: Option<usize>) -> Self {
        if cap == Some(0) {
            Wait::RendezvousSend
        } else {
            Wait::Send
        }
    }

    /// The `wait:` wait, by whether it has a send arm.
    pub(crate) fn select(has_send: bool) -> Self {
        if has_send {
            Wait::SelectSend
        } else {
            Wait::Select
        }
    }

    /// The guard wait of a box of kind `kind` (`"Shared"` or `"RwShared"`).
    pub(crate) fn guard(kind: &str) -> Self {
        if kind == "RwShared" {
            Wait::RwSharedGuard
        } else {
            Wait::SharedGuard
        }
    }

    /// The fault of a wait that cannot block where it is (a native callback with no thread of its
    /// own, a top level with no task that could ever answer): the phrase, then why.
    pub(crate) fn fault_text(self) -> String {
        let why = match self {
            Wait::Recv | Wait::Select => {
                "no runnable task can send. (Make sure a task that sends to this channel is \
                 spawned with `spawn:` and is still running.)"
            }
            Wait::Send | Wait::RendezvousSend | Wait::SelectSend => {
                "no runnable task can receive. (Make sure a task that receives from this channel \
                 is spawned with `spawn:` and is still running.)"
            }
            Wait::SharedGuard | Wait::RwSharedGuard => {
                "the task holding the guard is blocked and no runnable task can release it"
            }
            Wait::ExecutorJoin | Wait::Socket | Wait::Nursery => "no runnable task can unblock it",
        };
        format!("deadlock: {}: {why}", self.phrase())
    }
}

/// One blocked task of a deadlock report: its wait, where it waits, and its frames (innermost
/// first).
#[derive(Debug, Clone, PartialEq)]
pub struct BlockedTask {
    pub wait: String,
    pub span: Span,
    pub frames: Vec<TraceFrame>,
}

/// A deadlock fault's blocked tasks, in task order. Boxed on the error so a `RuntimeError` that
/// carries none stays one pointer wider.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub tasks: Vec<BlockedTask>,
    /// One task's own cut ([`cut`]): its one entry has no frames yet, and takes the frames of the
    /// task the fault ends ([`listing`]). `false` once folded by a join.
    own: bool,
}

/// The fault of one blocked task the verdict cut: the header, and the task's own `wait` at `span`.
/// `base` is the verdict's fault, whose markers it keeps. THE one way a deadlock fault is sited.
pub(super) fn cut(base: &RuntimeError, wait: &str, span: Span) -> RuntimeError {
    let mut e = base.clone();
    e.message = header(1);
    e.span = span;
    e.blocked = Some(Box::new(Blocked {
        tasks: vec![BlockedTask {
            wait: wait.to_string(),
            span,
            frames: Vec::new(),
        }],
        own: true,
    }));
    e
}

/// The report's first line, for `n` blocked tasks.
pub fn header(n: usize) -> String {
    if n == 1 {
        "deadlock: 1 task is blocked and cannot proceed".to_string()
    } else {
        format!("deadlock: {n} tasks are blocked and none can proceed")
    }
}

/// The blocked tasks a deadlock verdict's fault reports: its folded listing, or, for one task's own
/// cut, that task with `trace` (the frames of the task the fault ends). Empty for every other
/// fault, a `deadlock`-stamped one included (a fault that ends a run the verdict halted keeps its
/// own text, DEC-135).
pub fn listing(err: &RuntimeError, trace: &[TraceFrame]) -> Vec<BlockedTask> {
    match &err.blocked {
        Some(b) if b.own => b
            .tasks
            .iter()
            .map(|t| BlockedTask {
                frames: trace.to_vec(),
                ..t.clone()
            })
            .collect(),
        Some(b) => b.tasks.clone(),
        None => Vec::new(),
    }
}

/// The blocked tasks of several deadlock faults, in the order given: each part is a fault and the
/// frames of the task it ends ([`listing`]). The one way two listings are joined before a [`fold`].
pub(super) fn listings(parts: &[(&RuntimeError, &[TraceFrame])]) -> Vec<BlockedTask> {
    parts
        .iter()
        .flat_map(|(e, trace)| listing(e, trace))
        .collect()
}

/// Fold the blocked tasks of a join's deadlock slots, in task order, into one fault. `first` is the
/// lowest-index deadlock fault, whose markers the folded fault keeps.
pub(super) fn fold(first: RuntimeError, tasks: Vec<BlockedTask>) -> RuntimeError {
    let mut e = first;
    e.message = header(tasks.len());
    if let Some(t) = tasks.first() {
        e.span = t.span;
    }
    e.blocked = Some(Box::new(Blocked { tasks, own: false }));
    e
}

/// The rendered entries of a listing: `  task k: <wait> at <site>`, then the task's frames as
/// [`super::format_frames`] renders them, indented one level more.
pub fn render(tasks: &[BlockedTask], files: &[(u32, std::path::PathBuf)]) -> Vec<String> {
    let path_for = |file: u32| {
        files
            .iter()
            .find(|(f, _)| *f == file)
            .map(|(_, p)| p.as_path())
    };
    let mut out = Vec::new();
    for (i, t) in tasks.iter().enumerate() {
        out.push(format!(
            "  task {}: {} at {}",
            i + 1,
            t.wait,
            crate::lexer::render_span(t.span, path_for(t.span.file))
        ));
        for entry in super::format_frames(&t.frames, files) {
            out.push(entry.replace("\n  ", "\n    ").replacen("  ", "    ", 1));
        }
    }
    out
}
