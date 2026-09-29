//! HM3 retained mutation and readonly reconciliation on the canonical reactor.
use super::*;
use asura_storage::memory::{self, MemoryCreateResult};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Mutation,
    ResolveReady,
    Resolve,
    Done,
}
pub(super) struct Mutation {
    pub intent: journal::MemoryCreateIntent,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    phase: Phase,
    ticket: Option<writer::Ticket>,
    response: Option<Result<MemoryCreateResult, ()>>,
    response_consumed: bool,
}
impl Mutation {
    pub fn new(intent: journal::MemoryCreateIntent) -> Self {
        Self {
            intent,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            phase: Phase::ResolveReady,
            ticket: None,
            response: None,
            response_consumed: false,
        }
    }
    pub fn start(&mut self, writer: &writer::WriterHandle, body: String, deadline: Instant) {
        self.phase = Phase::Mutation;
        match writer.try_submit_before(
            writer::Command::MemoryCreate {
                turn: self.intent.operation,
                generation: self.intent.generation,
                ordinal: self.intent.ordinal,
                body,
                cancel: self.cancel.clone(),
            },
            deadline,
        ) {
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
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.response_consumed {
            None
        } else {
            self.ticket.as_ref().map(writer::Ticket::deadline)
        }
    }
    pub fn poll(
        &mut self,
        writer: &writer::WriterHandle,
        now: Instant,
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
            self.ticket = Some(
                writer
                    .try_submit_before(
                        writer::Command::MemoryResolveCreate {
                            turn: self.intent.operation,
                            generation: self.intent.generation,
                            ordinal: self.intent.ordinal,
                        },
                        now + Duration::from_secs(2),
                    )
                    .map_err(|_| {
                        self.phase = Phase::Done;
                    })?,
            );
        }
        Ok(None)
    }
    fn result(&self, outcome: MemoryCreateResult, absent_status: u8) -> journal::ToolResult {
        let committed = matches!(outcome, MemoryCreateResult::Committed(_));
        journal::ToolResult {
            operation: self.intent.operation,
            generation: self.intent.generation,
            ordinal: self.intent.ordinal,
            status: if committed { 1 } else { absent_status },
            text: if committed {
                self.intent.success_text()
            } else {
                String::new()
            },
            truncated: false,
            next_offset: None,
        }
    }
}

pub(super) fn prepare(
    replay: &journal::Replay,
    call: &tools::Call,
    project: journal::Id,
) -> Result<Mutation, tools::Rejection> {
    let tools::Arguments::MemoryCreateNote {
        body,
        source_version,
    } = &call.arguments
    else {
        return Err(tools::Rejection::InvalidArguments);
    };
    call.arguments.validate()?;
    let binding = memory::Binding {
        installation_id: memory::Id::new(replay.installation_id)
            .map_err(|_| tools::Rejection::Denied)?,
        graph_id: memory::Id::new(replay.initialization.graph)
            .map_err(|_| tools::Rejection::Denied)?,
        init_operation_id: memory::Id::new(replay.initialization.request)
            .map_err(|_| tools::Rejection::Denied)?,
    };
    let (intent, _) = memory::prepare_create(
        binding,
        memory::Id::new(project).map_err(|_| tools::Rejection::Denied)?,
        call.operation,
        call.generation,
        call.ordinal,
        body.clone(),
        source_version
            .as_deref()
            .map(tools::memory_id)
            .transpose()?,
    )
    .map_err(|_| tools::Rejection::InvalidArguments)?;
    Ok(Mutation::new(intent))
}
impl Owner {
    pub(super) fn capture_memory_create(&mut self, now: Instant) {
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
        if active.cancel.is_some() || active.terminal.is_some() || now >= tool.deadline {
            create.cancel();
        }
        let status = if active.cancel.is_some() {
            6
        } else if now >= tool.deadline {
            5
        } else {
            4
        };
        match create.poll(writer, now, status) {
            Ok(Some(result)) => tool.result = Some(result),
            Ok(None) => (),
            Err(()) => self.unavailable = Some("memory_write_outcome_unconfirmed"),
        }
    }
    pub(super) fn drive_create_recovery(&mut self, now: Instant) -> bool {
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
        match recovery.poll(writer, now, 4) {
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
