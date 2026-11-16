//! The solver adapter (security-policy.md, PL3 solver adapter; finding F4):
//! SymCC's `Solver` trait over an injected [`SolverPipe`]. A query is
//! collected in a bounded in-memory buffer and sent in one exchange, which
//! the pipe owner writes while it reads both outputs, so neither process
//! waits on a full pipe. Every call does its pipe work at once and returns
//! a ready future; [`block_on`] polls a SymCC future once.
use crate::{
    MAX_MODEL_BYTES, MAX_QUERY_BYTES, MAX_STDERR_KEPT, PipeEnd, PipeFlow, PipeStream, SolverCause,
    SolverPipe,
};
use cedar_policy_symcc::solver::{Decision, DecisionWithModel, Solver, SolverError};
use std::future::{Future, ready};
use std::io;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

/// Polls `future` once with a no-op waker. The adapter never waits, so a
/// future that is not ready is a protocol fault.
pub(crate) fn block_on<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// The query buffer: SymCC writes one query here. A write past 64 MiB
/// fails, so the query never reaches the pipe (SA-T6).
#[derive(Default)]
pub(crate) struct QueryBuffer {
    bytes: Vec<u8>,
    overflow: bool,
}
impl QueryBuffer {
    fn push(&mut self, bytes: &[u8]) -> bool {
        if self.overflow || self.bytes.len() + bytes.len() > MAX_QUERY_BYTES {
            self.overflow = true;
            return false;
        }
        self.bytes.extend_from_slice(bytes);
        true
    }
}
impl tokio::io::AsyncWrite for QueryBuffer {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.get_mut().push(buf) {
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Ready(Err(io::Error::other("input_bound")))
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The shape of the answer an exchange waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// One line: `sat`, `unsat`, `unknown` or an error.
    Line,
    /// A line `(`, then lines up to a line `)`; or one other line.
    Model,
}

/// The standard output reader of one exchange, with the 1 MiB bounds.
#[derive(Debug)]
pub(crate) struct Reader {
    shape: Shape,
    current: Vec<u8>,
    pub(crate) lines: Vec<String>,
    total: usize,
    pub(crate) done: bool,
    /// Bytes after the complete answer: drained and counted, not kept.
    pub(crate) extra: u64,
    pub(crate) fault: Option<SolverCause>,
}
impl Reader {
    pub(crate) fn new(shape: Shape) -> Self {
        Self {
            shape,
            current: Vec::new(),
            lines: Vec::new(),
            total: 0,
            done: false,
            extra: 0,
            fault: None,
        }
    }
    pub(crate) fn flow(&self) -> PipeFlow {
        if self.fault.is_some() {
            PipeFlow::Fault
        } else if self.done {
            PipeFlow::Answered
        } else {
            PipeFlow::More
        }
    }
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> PipeFlow {
        let mut rest = bytes;
        while !rest.is_empty() && self.fault.is_none() {
            if self.done {
                self.extra += rest.len() as u64;
                break;
            }
            let (chunk, end) = match rest.iter().position(|b| *b == b'\n') {
                Some(at) => (&rest[..at], Some(at)),
                None => (rest, None),
            };
            self.total += chunk.len() + usize::from(end.is_some());
            if self.current.len() + chunk.len() > MAX_MODEL_BYTES || self.total > MAX_MODEL_BYTES {
                self.fault = Some(SolverCause::OutputBound);
                break;
            }
            self.current.extend_from_slice(chunk);
            match end {
                Some(at) => {
                    let line =
                        String::from_utf8_lossy(&std::mem::take(&mut self.current)).into_owned();
                    self.line(line);
                    rest = &rest[at + 1..];
                }
                None => rest = &[],
            }
        }
        self.flow()
    }
    fn line(&mut self, line: String) {
        let trimmed = line.trim().to_owned();
        let first = self.lines.is_empty();
        self.lines.push(line);
        self.done = match self.shape {
            Shape::Line => true,
            Shape::Model => (first && trimmed != "(") || (!first && trimmed == ")"),
        };
    }
}

/// SymCC's `Solver` over the injected pipe.
pub(crate) struct Adapter<'p> {
    pipe: &'p mut dyn SolverPipe,
    buffer: QueryBuffer,
    /// The first fault; after it no exchange runs.
    pub(crate) cause: Option<SolverCause>,
    /// The first 64 KiB of standard error.
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_dropped: u64,
    pub(crate) exchanges: usize,
}
impl<'p> Adapter<'p> {
    pub(crate) fn new(pipe: &'p mut dyn SolverPipe) -> Self {
        Self {
            pipe,
            buffer: QueryBuffer::default(),
            cause: None,
            stderr: Vec::new(),
            stderr_dropped: 0,
            exchanges: 0,
        }
    }
    /// SymCC wrote past the 64 MiB query bound.
    pub(crate) fn overflowed(&self) -> bool {
        self.buffer.overflow
    }
    fn fail(&mut self, cause: SolverCause) -> SolverError {
        self.cause.get_or_insert(cause);
        SolverError::Solver(cause.as_str().to_owned())
    }
    /// One exchange: the buffered query plus `tail`, then the answer.
    fn exchange(&mut self, tail: &[u8], shape: Shape) -> Result<Vec<String>, SolverError> {
        if self.cause.is_some() {
            return Err(SolverError::SolverMarkedFailed);
        }
        if !self.buffer.push(tail) {
            return Err(self.fail(SolverCause::InputBound));
        }
        let input = std::mem::take(&mut self.buffer.bytes);
        let mut reader = Reader::new(shape);
        let stderr = &mut self.stderr;
        let dropped = &mut self.stderr_dropped;
        self.exchanges += 1;
        let result = self
            .pipe
            .exchange(&input, &mut |stream, bytes| match stream {
                PipeStream::Stdout => reader.feed(bytes),
                PipeStream::Stderr => {
                    let room = MAX_STDERR_KEPT.saturating_sub(stderr.len());
                    let kept = room.min(bytes.len());
                    stderr.extend_from_slice(&bytes[..kept]);
                    *dropped += (bytes.len() - kept) as u64;
                    reader.flow()
                }
            });
        let cause = match result {
            Ok(()) if reader.fault.is_none() && reader.done && reader.extra == 0 => {
                return Ok(reader.lines);
            }
            Ok(()) => reader.fault.unwrap_or(SolverCause::Protocol),
            Err(PipeEnd::Fault) => reader.fault.unwrap_or(SolverCause::Protocol),
            Err(PipeEnd::Closed) => SolverCause::Closed,
            Err(PipeEnd::Cancelled) => SolverCause::Cancelled,
            Err(PipeEnd::MemoryBound) => SolverCause::MemoryBound,
            Err(PipeEnd::Io) => SolverCause::Io,
        };
        Err(self.fail(cause))
    }
    fn decision(&mut self, lines: &[String]) -> Result<Decision, SolverError> {
        let line = lines.first().map_or("", |l| l.trim());
        match line {
            "sat" => Ok(Decision::Sat),
            "unsat" => Ok(Decision::Unsat),
            "unknown" => Ok(Decision::Unknown),
            _ => Err(self.fail(SolverCause::Protocol)),
        }
    }
    fn check(&mut self) -> Result<Decision, SolverError> {
        let lines = self.exchange(b"(check-sat)\n", Shape::Line)?;
        self.decision(&lines)
    }
    fn check_with_model(&mut self) -> Result<DecisionWithModel, SolverError> {
        match self.check()? {
            Decision::Unsat => Ok(DecisionWithModel::Unsat),
            Decision::Unknown => Ok(DecisionWithModel::Unknown),
            Decision::Sat => {
                let lines = self.exchange(b"(get-model)\n", Shape::Model)?;
                if lines.first().map(|l| l.trim()) != Some("(") || lines.len() < 2 {
                    return Err(self.fail(SolverCause::Protocol));
                }
                let mut model = String::new();
                for line in lines {
                    model.push_str(&line);
                    model.push('\n');
                }
                Ok(DecisionWithModel::Sat { model })
            }
        }
    }
}
impl Solver for Adapter<'_> {
    fn smtlib_input(&mut self) -> &mut (dyn tokio::io::AsyncWrite + Unpin + Send) {
        &mut self.buffer
    }
    fn enable_models(&mut self) -> impl Future<Output = Result<(), SolverError>> + Send {
        let result = if self.buffer.push(b"(set-option :produce-models true)\n") {
            Ok(())
        } else {
            Err(self.fail(SolverCause::InputBound))
        };
        ready(result)
    }
    fn check_sat(&mut self) -> impl Future<Output = Result<Decision, SolverError>> + Send {
        ready(self.check())
    }
    fn check_sat_with_model(
        &mut self,
    ) -> impl Future<Output = Result<DecisionWithModel, SolverError>> + Send {
        ready(self.check_with_model())
    }
}
