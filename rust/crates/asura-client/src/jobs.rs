//! Typed job calls (tool-job-controller.md, J4 control messages). Each wait
//! has no time bound: it ends on the reply, the caller's `cancelled`, this
//! attachment's cancel or a lost connection. Reply `error` tokens are returned
//! in the reply for the caller to show; only a reply that does not answer the
//! request is a protocol error.
use crate::{Client, Error, Result};
use asura_control::{jobs::MAX_JOB_LIST_ENTRIES, pb};
use pb::envelope::Body;

impl Client {
    /// True when the service listed `capability` in its `HelloReply`. A client
    /// that does not see `CAPABILITY_JOBS` hides the job commands.
    pub fn supports(&self, capability: pb::Capability) -> bool {
        self.capabilities.contains(&(capability as i32))
    }

    /// One `JobList` page, or a parked list when `after_revision` is set:
    /// the service answers when the job revision differs.
    pub fn jobs(
        &mut self,
        request: pb::JobList,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobListReply> {
        let expected = request.clone();
        match self.request_cancellable(Body::JobList(request), cancelled)? {
            Body::JobListReply(reply) if list_matches(&expected, &reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// One job's facts, command, cwd and notes.
    pub fn inspect_job(
        &mut self,
        job: [u8; 16],
        project: Option<[u8; 16]>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobDetail> {
        let request = pb::JobInspect {
            job_id: Some(job.to_vec()),
            project_id: project.map(|p| p.to_vec()),
        };
        match self.request_cancellable(Body::JobInspect(request), cancelled)? {
            Body::JobDetail(reply)
                if reply.error.is_some() || entry_matches(reply.entry.as_ref(), job, project) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }

    /// One page of one stream.
    pub fn read_job(
        &mut self,
        request: pb::JobRead,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobPage> {
        let expected = request.clone();
        match self.request_cancellable(Body::JobRead(request), cancelled)? {
            Body::JobPage(reply) if read_matches(&expected, &reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// A parked observation: the service answers when the stream passes
    /// `offset`, the state differs from `after_state`, or the job settles.
    /// Use an attachment of its own: an attachment has one pending request.
    pub fn observe_job(
        &mut self,
        request: pb::JobObserve,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobPage> {
        let expected = request.clone();
        match self.request_cancellable(Body::JobObserve(request), cancelled)? {
            Body::JobPage(reply) if observe_matches(&expected, &reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Requests cancellation (with `kill` for a person job). The service
    /// replies after its record; a cancelled or lost wait is
    /// `OutcomeUnconfirmed`, and the caller may list again: cancel is
    /// idempotent.
    pub fn cancel_job(
        &mut self,
        request: pb::JobCancel,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobCancelReply> {
        let expected = request.clone();
        match self.mutation(Body::JobCancel(request), cancelled)? {
            Body::JobCancelReply(reply)
                if reply.request_id == expected.request_id
                    && (reply.error.is_some()
                        || mutation_entry_matches(
                            reply.entry.as_ref(),
                            &expected.job_id,
                            &expected.expected_project_id,
                        )) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }

    /// Pin, unpin, delete or note. The service replies after the kind 30
    /// commit; a cancelled or lost wait is `OutcomeUnconfirmed` (a repeated
    /// note after a lost reply can add a second note).
    pub fn retain_job(
        &mut self,
        request: pb::JobRetain,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::JobRetainReply> {
        let expected = request.clone();
        match self.mutation(Body::JobRetain(request), cancelled)? {
            Body::JobRetainReply(reply)
                if reply.request_id == expected.request_id
                    && (reply.error.is_some()
                        || mutation_entry_matches(
                            reply.entry.as_ref(),
                            &expected.job_id,
                            &expected.expected_project_id,
                        )) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
}

/// A `JobListReply` answers its request: no more entries than asked, each in
/// the requested scope and cursor range, and the EX11 value exactly for the
/// project and conversation scopes.
pub(crate) fn list_matches(request: &pb::JobList, reply: &pb::JobListReply) -> bool {
    if reply.error.is_some() {
        return true;
    }
    let limit = request.limit.unwrap_or(MAX_JOB_LIST_ENTRIES) as usize;
    let all = request.scope == Some(pb::JobScope::AllProjects as i32);
    reply.entries.len() <= limit
        && reply.execution_enabled.is_some() != all
        && reply.entries.iter().all(|e| {
            (all || e.project_id == request.project_id)
                && (request.conversation_id.is_none()
                    || e.conversation_id == request.conversation_id)
                && (request.live_only != Some(true)
                    || e.state != Some(pb::JobState::Settled as i32))
                && request
                    .before
                    .is_none_or(|before| e.sequence.is_some_and(|s| s < before))
        })
}

fn entry_matches(entry: Option<&pb::JobEntry>, job: [u8; 16], project: Option<[u8; 16]>) -> bool {
    entry.is_some_and(|e| {
        e.job_id.as_deref() == Some(job.as_slice())
            && project.is_none_or(|p| e.project_id.as_deref() == Some(p.as_slice()))
    })
}

fn mutation_entry_matches(
    entry: Option<&pb::JobEntry>,
    job: &Option<Vec<u8>>,
    project: &Option<Vec<u8>>,
) -> bool {
    entry.is_some_and(|e| &e.job_id == job && &e.project_id == project)
}

fn page_entry(
    request_job: &Option<Vec<u8>>,
    project: &Option<Vec<u8>>,
    reply: &pb::JobPage,
) -> bool {
    reply.entry.as_ref().is_some_and(|e| {
        &e.job_id == request_job && (project.is_none() || &e.project_id == project)
    })
}

pub(crate) fn read_matches(request: &pb::JobRead, reply: &pb::JobPage) -> bool {
    if reply.error.is_some() {
        return true;
    }
    let limit = u64::from(
        request
            .limit
            .unwrap_or(asura_control::jobs::MAX_JOB_PAGE_BYTES),
    );
    page_entry(&request.job_id, &request.project_id, reply)
        && reply.pages.len() == 1
        && reply.pages[0].stream == request.stream
        && reply.pages[0]
            .next_offset
            .zip(reply.pages[0].start)
            .is_some_and(|(next, start)| next.checked_sub(start).is_some_and(|n| n <= limit))
}

pub(crate) fn observe_matches(request: &pb::JobObserve, reply: &pb::JobPage) -> bool {
    if reply.error.is_some() {
        return true;
    }
    page_entry(&request.job_id, &request.project_id, reply)
        && match request.stream {
            Some(stream) => reply.pages.len() == 1 && reply.pages[0].stream == Some(stream),
            // Both streams at the end; the codec checked they differ.
            None => !reply.pages.is_empty(),
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(project: u8, conversation: u8, sequence: u64, state: pb::JobState) -> pb::JobEntry {
        pb::JobEntry {
            job_id: Some(vec![sequence as u8; 16]),
            project_id: Some(vec![project; 16]),
            conversation_id: Some(vec![conversation; 16]),
            state: Some(state as i32),
            sequence: Some(sequence),
            ..Default::default()
        }
    }
    fn list(scope: pb::JobScope) -> pb::JobList {
        pb::JobList {
            scope: Some(scope as i32),
            project_id: (scope != pb::JobScope::AllProjects).then(|| vec![1; 16]),
            conversation_id: (scope == pb::JobScope::Conversation).then(|| vec![2; 16]),
            ..Default::default()
        }
    }
    fn reply(entries: Vec<pb::JobEntry>, execution: Option<bool>) -> pb::JobListReply {
        pb::JobListReply {
            entries,
            execution_enabled: execution,
            ..Default::default()
        }
    }
    #[test]
    fn list_reply_must_answer_its_scope_limit_and_cursor() {
        use pb::{JobScope as Scope, JobState as State};
        let mine = entry(1, 2, 9, State::Running);
        assert!(list_matches(
            &list(Scope::Project),
            &reply(
                vec![mine.clone(), entry(1, 3, 8, State::Settled)],
                Some(true)
            )
        ));
        // EX11 value exactly for project and conversation scopes.
        assert!(!list_matches(
            &list(Scope::Project),
            &reply(Vec::new(), None)
        ));
        assert!(!list_matches(
            &list(Scope::AllProjects),
            &reply(Vec::new(), Some(true))
        ));
        assert!(list_matches(
            &list(Scope::AllProjects),
            &reply(vec![entry(7, 7, 4, State::Running)], None)
        ));
        // Another project's or conversation's job.
        assert!(!list_matches(
            &list(Scope::Project),
            &reply(vec![entry(7, 2, 9, State::Running)], Some(true))
        ));
        assert!(!list_matches(
            &list(Scope::Conversation),
            &reply(vec![entry(1, 3, 9, State::Running)], Some(false))
        ));
        // Live only, cursor and limit.
        let mut request = list(Scope::Project);
        request.live_only = Some(true);
        assert!(!list_matches(
            &request,
            &reply(vec![entry(1, 2, 9, State::Settled)], Some(true))
        ));
        let mut request = list(Scope::Project);
        request.before = Some(9);
        assert!(!list_matches(
            &request,
            &reply(vec![mine.clone()], Some(true))
        ));
        request.before = Some(10);
        assert!(list_matches(
            &request,
            &reply(vec![mine.clone()], Some(true))
        ));
        request.limit = Some(1);
        assert!(!list_matches(
            &request,
            &reply(
                vec![mine.clone(), entry(1, 2, 8, State::Running)],
                Some(true)
            )
        ));
        // An error token answers any request.
        let error = pb::JobListReply {
            error: Some("busy".into()),
            ..Default::default()
        };
        assert!(list_matches(&list(Scope::Project), &error));
    }
    #[test]
    fn page_replies_must_answer_the_job_and_stream() {
        let job = Some(vec![5; 16]);
        let page = |stream: pb::JobStream, start: u64, next: u64| pb::JobStreamPage {
            stream: Some(stream as i32),
            start: Some(start),
            next_offset: Some(next),
            ..Default::default()
        };
        let reply = |pages| pb::JobPage {
            entry: Some(pb::JobEntry {
                job_id: job.clone(),
                project_id: Some(vec![1; 16]),
                ..Default::default()
            }),
            pages,
            error: None,
        };
        let read = pb::JobRead {
            job_id: job.clone(),
            project_id: Some(vec![1; 16]),
            stream: Some(pb::JobStream::Stdout as i32),
            offset: Some(0),
            limit: Some(10),
            at_end: None,
        };
        assert!(read_matches(
            &read,
            &reply(vec![page(pb::JobStream::Stdout, 0, 10)])
        ));
        assert!(!read_matches(
            &read,
            &reply(vec![page(pb::JobStream::Stdout, 0, 11)])
        ));
        assert!(!read_matches(
            &read,
            &reply(vec![page(pb::JobStream::Stderr, 0, 1)])
        ));
        let mut other = read.clone();
        other.job_id = Some(vec![6; 16]);
        assert!(!read_matches(
            &other,
            &reply(vec![page(pb::JobStream::Stdout, 0, 1)])
        ));
        let mut other = read.clone();
        other.project_id = Some(vec![2; 16]);
        assert!(!read_matches(
            &other,
            &reply(vec![page(pb::JobStream::Stdout, 0, 1)])
        ));
        let observe = pb::JobObserve {
            job_id: job.clone(),
            project_id: None,
            stream: None,
            offset: None,
            at_end: Some(true),
            after_state: None,
        };
        let both = vec![
            page(pb::JobStream::Stdout, 0, 1),
            page(pb::JobStream::Stderr, 0, 1),
        ];
        assert!(observe_matches(&observe, &reply(both.clone())));
        let one = pb::JobObserve {
            stream: Some(pb::JobStream::Stderr as i32),
            offset: Some(0),
            at_end: None,
            ..observe
        };
        assert!(!observe_matches(&one, &reply(both)));
        assert!(observe_matches(
            &one,
            &reply(vec![page(pb::JobStream::Stderr, 0, 1)])
        ));
    }
}
