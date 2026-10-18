//! HM3 retained mutation and readonly reconciliation on the canonical reactor.
use super::*;
use asura_storage::memory::MemoryCreateResult;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Mutation,
    ResolveReady,
    Resolve,
    Done,
}
pub(super) struct Mutation {
    /// The derived identity of the `run` create_note call (HM3, TCR-D2).
    pub intent: journal::CreateNoteCall,
    /// The optional source version of the service's parse; the body is
    /// handed to `start` and never retained here.
    source: Option<journal::Id>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    phase: Phase,
    ticket: Option<writer::Ticket>,
    response: Option<Result<MemoryCreateResult, ()>>,
    response_consumed: bool,
}
impl Mutation {
    /// A mutation that only resolves (restart recovery): no source is needed.
    pub fn new(intent: journal::CreateNoteCall) -> Self {
        Self::with_source(intent, None)
    }
    fn with_source(intent: journal::CreateNoteCall, source: Option<journal::Id>) -> Self {
        Self {
            intent,
            source,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            phase: Phase::ResolveReady,
            ticket: None,
            response: None,
            response_consumed: false,
        }
    }
    /// The write has no time bound (J1-0): it ends on its reply or the
    /// turn's cancel, which stops the writer's cancellable put.
    pub fn start(&mut self, writer: &writer::WriterHandle, body: String) {
        self.phase = Phase::Mutation;
        match writer.try_submit(writer::Command::MemoryCreate {
            turn: self.intent.operation,
            generation: self.intent.generation,
            ordinal: self.intent.ordinal,
            body,
            source: self.source,
            cancel: self.cancel.clone(),
        }) {
            Ok(ticket) => self.ticket = Some(ticket),
            Err(_) => self.phase = Phase::ResolveReady,
        }
    }
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn settled(&self) -> bool {
        self.ticket.as_ref().is_none_or(writer::Ticket::is_settled)
    }
    pub fn poll(
        &mut self,
        writer: &writer::WriterHandle,
        absent_status: u8,
    ) -> Result<Option<journal::ToolResult>, ()> {
        if self.phase == Phase::Done {
            return Ok(None);
        }
        if let Some(ticket) = &mut self.ticket {
            if !self.response_consumed
                && let Some(reply) = ticket.poll()
            {
                self.response_consumed = true;
                if self.phase == Phase::Resolve {
                    self.response =
                        Some(reply.map_err(|_| ()).and_then(|reply| {
                            reply.memory_create_result.ok_or(())?.map_err(|_| ())
                        }));
                }
            }
            if !ticket.is_settled() {
                return Ok(None);
            }
            // Acquire of settlement orders the worker's prior response publication.
            if !self.response_consumed
                && let Some(reply) = ticket.poll()
            {
                self.response_consumed = true;
                if self.phase == Phase::Resolve {
                    self.response =
                        Some(reply.map_err(|_| ()).and_then(|reply| {
                            reply.memory_create_result.ok_or(())?.map_err(|_| ())
                        }));
                }
            }
            self.ticket = None;
            if self.phase == Phase::Mutation {
                self.phase = Phase::ResolveReady;
                self.response_consumed = false;
            } else {
                self.phase = Phase::Done;
                let outcome = self.response.take().ok_or(())??;
                return Ok(Some(self.result(outcome, absent_status)));
            }
        }
        if self.phase == Phase::ResolveReady {
            self.phase = Phase::Resolve;
            self.response_consumed = false;
            // A journal-writer read: it waits for the real reply (J1-0).
            self.ticket = Some(
                writer
                    .try_submit(writer::Command::MemoryResolveCreate {
                        turn: self.intent.operation,
                        generation: self.intent.generation,
                        ordinal: self.intent.ordinal,
                    })
                    .map_err(|_| {
                        self.phase = Phase::Done;
                    })?,
            );
        }
        Ok(None)
    }
    /// Kind 15 stores no text for create_note; the text sent to the helper
    /// is rendered from the derived identity (TCR-D1).
    fn result(&self, outcome: MemoryCreateResult, absent_status: u8) -> journal::ToolResult {
        let committed = matches!(outcome, MemoryCreateResult::Committed(_));
        journal::ToolResult {
            operation: self.intent.operation,
            generation: self.intent.generation,
            ordinal: self.intent.ordinal,
            status: if committed { 1 } else { absent_status },
            text: String::new(),
            truncated: false,
            next_offset: None,
        }
    }
}

/// Derives the call identity for a `run` create_note decision. A binding or
/// project that cannot be named is a denial before the record (TCR-D10).
pub(super) fn prepare(
    replay: &journal::Replay,
    call: &tools::Call,
    project: journal::Id,
) -> Result<Mutation, tools::Rejection> {
    let tools::Arguments::MemoryCreateNote { source_version, .. } = &call.arguments else {
        return Err(tools::Rejection::InvalidArguments);
    };
    call.arguments.validate()?;
    let source = source_version
        .as_deref()
        .map(tools::memory_id)
        .transpose()?
        .map(|id| id.bytes());
    let intent = journal::CreateNoteCall::derive(
        [
            replay.installation_id,
            replay.initialization.graph,
            replay.initialization.request,
        ],
        project,
        call.operation,
        call.generation,
        call.ordinal,
    )
    .map_err(|_| tools::Rejection::Denied)?;
    Ok(Mutation::with_source(intent, source))
}
impl Owner {
    pub(super) fn capture_memory_create(&mut self) {
        if matches!(self.pending, Some((_, Job::ToolResult))) {
            return;
        }
        let Some(writer) = &self.writer else {
            return;
        };
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(tool) = &mut active.tool else {
            return;
        };
        if !tool.intent_committed || !tool.dispatched || tool.result.is_some() {
            return;
        }
        let Some(create) = &mut tool.create else {
            return;
        };
        if active.cancel.is_some() || active.terminal.is_some() {
            create.cancel();
        }
        let status = if active.cancel.is_some() { 6 } else { 4 };
        match create.poll(writer, status) {
            Ok(Some(result)) => tool.result = Some(result),
            Ok(None) => (),
            Err(()) => self.unavailable = Some("memory_write_outcome_unconfirmed"),
        }
    }
    pub(super) fn drive_create_recovery(&mut self) -> bool {
        if self.create_recovery.is_none() {
            return false;
        }
        if self.pending.is_some() || self.unavailable.is_some() {
            return true;
        }
        let Some(writer) = &self.writer else {
            return true;
        };
        let recovery = self.create_recovery.as_mut().expect("recovery");
        match recovery.poll(writer, 4) {
            Ok(Some(result)) => {
                if let Err(error) = self.append(
                    journal::Record::ToolResult(result),
                    Job::RecoverCreateResult,
                    None,
                ) {
                    self.unavailable = Some(storage_error(error));
                }
            }
            Ok(None) => (),
            Err(()) => self.unavailable = Some("memory_write_outcome_unconfirmed"),
        }
        true
    }
}

#[cfg(test)]
#[path = "memory_create_tests.rs"]
mod tests;
