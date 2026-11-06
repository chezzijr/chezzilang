//! VM: the one deadlock report: each wait's phrase, the header, and the listing of blocked tasks.
//!
//! Go's model (`fatal error: all goroutines are asleep - deadlock!`, then every goroutine with its
//! wait and stack). A verdict cuts every blocked task; each cut task ends with a deadlock fault
//! whose text is its own wait ([`Wait::phrase`]) at its own site, and adds its own entry to the
//! run's [`Listing`] at that cut, keyed by its place in the spawn tree ([`TaskPath`]). The listing
//! is THE report: a driver takes it once, after every join and the Executor drain
//! ([`super::Vm::take_listing`]), and renders it through [`render`].

use super::{ProtoId, RuntimeError, TraceFrame};
use crate::ast::Span;
use std::collections::BTreeMap;
use std::sync::Mutex;

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

/// A deadlock fault's blocked tasks, in spawn-tree order. Boxed on the error so a `RuntimeError`
/// that carries none stays one pointer wider.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub tasks: Vec<BlockedTask>,
    /// One task's own cut ([`cut`]): its one entry has no frames yet, and takes the frames of the
    /// task the fault ends ([`listing`]). `false` once the run's listing replaced it
    /// ([`super::Vm::list_blocked`]).
    own: bool,
    /// The cut task's key in the run's [`Listing`], when the cut added it there: a later report
    /// of this fault finds the task already listed ([`super::Vm::list_blocked`]).
    key: Option<Vec<Step>>,
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
        key: None,
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

/// The blocked tasks a deadlock verdict's fault reports: the run's listing the driver attached
/// ([`super::Vm::list_blocked`]), or, for one task's own cut, that task with `trace` (the frames of
/// the task the fault ends). Empty for every other fault, a `deadlock`-stamped one included (a
/// fault that ends a run the verdict halted keeps its own text, DEC-135).
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

/// A task's place in the run's spawn tree: its parent's path and its index among the tasks and
/// Executors its parent made, in the order it made them. The run's own tasks sit under [`RUN`],
/// an Executor's jobs under [`JOBS`], by the Executor's place (its creator's path, then the
/// creator's index for it) then the job's submission index, so every job follows every task of
/// the run. A job's own path starts with [`JOBS`], so the jobs of an Executor a job made follow
/// every job of an Executor a run task made, and sort among themselves by the creating job's
/// place. Travels with its task ([`super::Vm::swap_task`]).
#[derive(Clone, Debug)]
pub(crate) struct TaskPath {
    path: Vec<u64>,
    /// The index of the next task or Executor this task makes. `u64`: a run never makes 2^64.
    next: u64,
}

const RUN: u64 = 0;
const JOBS: u64 = 1;

/// One step of a listing key: a child index of the path, or the task's own entry, which sorts
/// after every child of the same task (a task's own wait follows its nursery's tasks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Step {
    Child(u64),
    Own,
}

impl Default for TaskPath {
    /// The run's root task (`main`, a `test fn`).
    fn default() -> Self {
        TaskPath {
            path: vec![RUN],
            next: 0,
        }
    }
}

impl TaskPath {
    /// The next child's place: a task this task spawns, or the key of an Executor it creates.
    fn next_child(&mut self) -> Vec<u64> {
        let mut path = self.path.clone();
        path.push(self.next);
        self.next += 1;
        path
    }

    /// The path of the task this task spawns now.
    pub(super) fn child(&mut self) -> TaskPath {
        TaskPath {
            path: self.next_child(),
            next: 0,
        }
    }

    /// The key of the Executor this task creates now ([`TaskPath::job`]).
    pub(super) fn executor(&mut self) -> Vec<u64> {
        self.next_child()
    }

    /// The path of job `index` of the Executor keyed `executor`.
    pub(super) fn job(executor: &[u64], index: usize) -> TaskPath {
        let mut path = vec![JOBS];
        path.extend_from_slice(executor);
        path.push(u64::try_from(index).unwrap_or(u64::MAX));
        TaskPath { path, next: 0 }
    }
}

/// One entry of the run's listing: the wait, its site, and the task's frames (innermost first).
/// `reported` once a report took it ([`Listing::take`]); the entry stays, so the task is not
/// reported twice.
struct Entry {
    wait: &'static str,
    span: Span,
    frames: Vec<(ProtoId, Span)>,
    reported: bool,
}

/// THE deadlock listing of a run: one entry per task the verdict cut, added at the cut and keyed by
/// the task's [`TaskPath`], so the report lists each task once, in spawn-tree order, whichever task
/// a join or unwind reached first. One per run, shared by every sched of the run
/// (`QuiesceState::listing`).
#[derive(Default)]
pub(crate) struct Listing(Mutex<BTreeMap<Vec<Step>, Entry>>);

/// What [`Listing::take`] hands a report: one entry's wait, site and frames.
type Taken = (&'static str, Span, Vec<(ProtoId, Span)>);

impl Listing {
    /// The cut task at `task` waits on `wait` at `span`, with `frames`; returns its key. A task's
    /// own entry follows its children's (a nursery's tasks in place of the task that opened it,
    /// then its own wait). A task is listed once: the first cut stands.
    pub(super) fn add(
        &self,
        task: &TaskPath,
        wait: &'static str,
        span: Span,
        frames: Vec<(ProtoId, Span)>,
    ) -> Vec<Step> {
        let key: Vec<Step> = task
            .path
            .iter()
            .map(|&i| Step::Child(i))
            .chain([Step::Own])
            .collect();
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_insert(Entry {
                wait,
                span,
                frames,
                reported: false,
            });
        key
    }

    /// The fault of the task at `task`, cut on `wait` at `span` with `frames`, from the verdict's
    /// fault `base`: the task's entry ([`Listing::add`]), and the fault that names it. THE one way
    /// a cut task enters the listing.
    pub(super) fn cut(
        &self,
        base: &RuntimeError,
        task: &TaskPath,
        wait: &'static str,
        span: Span,
        frames: Vec<(ProtoId, Span)>,
    ) -> RuntimeError {
        let key = self.add(task, wait, span, frames);
        let mut e = base.at_site(wait, span);
        if let Some(b) = e.blocked.as_mut() {
            b.key = Some(key);
        }
        e
    }

    /// Every entry no report took yet, in spawn-tree order; each is reported from now on.
    fn take(&self) -> Vec<Taken> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.values_mut()
            .filter(|e| !e.reported)
            .map(|e| {
                e.reported = true;
                (e.wait, e.span, e.frames.clone())
            })
            .collect()
    }

    /// True when `key`'s task is in the listing and a report took it.
    fn reported(&self, key: &[Step]) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_some_and(|e| e.reported)
    }
}

impl super::Vm {
    /// The fault of this task's cut on `wait` at `span`, from the verdict's fault `base`: the one
    /// way a task the verdict cut in place enters the run's listing.
    pub(super) fn cut_at(
        &self,
        base: &RuntimeError,
        wait: &'static str,
        span: Span,
    ) -> RuntimeError {
        self.quiesce
            .listing
            .cut(base, &self.task, wait, span, self.raw_frames())
    }

    /// Take the run's listing: every entry no report took yet, in spawn-tree order, each entry's
    /// frames named. A `chezzi test` run so takes each test's listing alone, and reports each task
    /// once.
    pub fn take_listing(&self) -> Vec<BlockedTask> {
        self.quiesce
            .listing
            .take()
            .into_iter()
            .map(|(wait, span, frames)| BlockedTask {
                wait: wait.to_string(),
                span,
                frames: self.trace_of(&frames),
            })
            .collect()
    }

    /// A run's end, for a driver: a deadlock verdict's fault reports the run's listing, its first
    /// task's site and the header. A cut whose task an earlier report listed (a job a test's
    /// verdict cut, joined by the end-of-file drain), with no task left to list, is no fault: the
    /// task is reported once. Every other result is returned as is. The listing is taken either
    /// way.
    pub(crate) fn list_blocked(&self, r: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
        let tasks = self.take_listing();
        match r {
            Err(mut e) if e.blocked.is_some() && !tasks.is_empty() => {
                e.message = header(tasks.len());
                e.span = tasks[0].span;
                e.blocked = Some(Box::new(Blocked {
                    tasks,
                    own: false,
                    key: None,
                }));
                Err(e)
            }
            Err(e)
                if e.blocked.as_ref().is_some_and(|b| {
                    b.key
                        .as_ref()
                        .is_some_and(|k| self.quiesce.listing.reported(k))
                }) =>
            {
                Ok(())
            }
            r => r,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A child counter at the `u32` boundary neither wraps nor collides with its parent's own
    /// entry: both children are listed, before the parent.
    #[test]
    fn a_child_past_u32_max_is_listed_before_its_parent() {
        let mut parent = TaskPath {
            path: vec![RUN],
            next: u32::MAX.into(),
        };
        let (a, b) = (parent.child(), parent.child());
        let listing = Listing::default();
        for (task, wait) in [(&a, "a"), (&b, "b"), (&parent, "parent")] {
            listing.add(task, wait, Span::default(), Vec::new());
        }
        let waits: Vec<_> = listing.take().into_iter().map(|e| e.0).collect();
        assert_eq!(waits, ["a", "b", "parent"]);
    }
}
