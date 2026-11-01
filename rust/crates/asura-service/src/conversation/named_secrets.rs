//! Named secret requests in the conversation owner (named-secrets.md,
//! control API; envelope fields 73-75).
//!
//! `SecretPut` is the only control message that carries a value. The owner
//! moves the value into zeroizing memory before any other step, checks the
//! secret name rule, and hands the value to the secret store worker, the
//! only thread that makes Keychain calls (AE-01). No journal record, log
//! line, audit field or reply holds the value or the name's value; the
//! audit events carry reply codes only. At most [`IN_FLIGHT`] requests wait
//! at once, then `busy` (AE-03). No wait has a time bound: each ends by the
//! worker's reply, or by shutdown, which drops the pending replies.
use super::person_command::{AuditReply, delete_audit, failure_audit};
use super::{Body, Owner, reply};
use crate::secret_store::{
    DeleteReply, NamedListReply, NamedPutReply, Polled, Refused, StoreFailure, Ticket,
};
use asura_control::pb;
use asura_control::secrets::Zeroizing;
use asura_platform::secrets::named;
use asura_storage::audit::{Event, SecretStoreReply as StoreReply};

/// Named secret requests waiting for the worker at once.
pub(super) const IN_FLIGHT: usize = 4;
/// The |OSStatus| of a runtime with no Keychain target (`errSecNotAvailable`).
const NO_KEYCHAIN: u32 = 25_291;

pub(super) enum Waiting {
    Put(Ticket<NamedPutReply>),
    Remove(Ticket<DeleteReply>),
    List(Ticket<NamedListReply>),
}

/// One admitted request: its envelope (the value already removed) and the
/// worker's ticket.
pub(super) struct Pending {
    request: pb::Envelope,
    waiting: Waiting,
}

fn answer(request: pb::Envelope, result: pb::SecretResult, status: Option<u32>) -> pb::Envelope {
    reply(
        request,
        Body::SecretReply(pb::SecretReply {
            result: Some(result as i32),
            names: Vec::new(),
            status,
        }),
    )
}

/// The reply of a store failure: `not_signed` is its own result.
fn failure_result(failure: &StoreFailure) -> (pb::SecretResult, Option<u32>) {
    use pb::SecretResult as R;
    let status = failure.status().unsigned_abs();
    if failure.not_signed() {
        return (R::NotSigned, None);
    }
    let status = Some(if status == 0 { NO_KEYCHAIN } else { status });
    match failure {
        StoreFailure::Locked { .. } => (R::Locked, status),
        StoreFailure::Denied { .. } => (R::Denied, status),
        StoreFailure::Unavailable { .. } => (R::Unavailable, status),
    }
}

impl Owner {
    /// One `SecretPut`, `SecretList` or `SecretRemove`. The reply follows
    /// later through the outbox, or at once for a refusal.
    pub(super) fn secret_request(&mut self, mut request: pb::Envelope) -> Option<pb::Envelope> {
        use pb::SecretResult as R;
        // The value leaves the envelope first; it is zeroed on drop.
        let value = match request.body.as_mut() {
            Some(Body::SecretPut(v)) => Zeroizing::new(v.value.take().unwrap_or_default()),
            _ => Zeroizing::new(Vec::new()),
        };
        if self.closing {
            return Some(super::error(request, "service_draining"));
        }
        let name = match request.body.as_ref() {
            Some(Body::SecretPut(v)) => v.name.clone(),
            Some(Body::SecretRemove(v)) => v.name.clone(),
            _ => None,
        };
        if let Some(name) = &name
            && !named::name_valid(name)
        {
            return Some(answer(request, R::Invalid, None));
        }
        if self.persons.named.len() >= IN_FLIGHT {
            return Some(answer(request, R::Busy, None));
        }
        let Some(store) = &self.persons.store else {
            // No target in this runtime, or the worker could not start:
            // unavailable. The start-time read or replay still pending:
            // busy, so the person tries again.
            let none = matches!(self.persons.target, Some(None)) || self.persons.store_failed;
            return Some(if none {
                answer(request, R::Unavailable, Some(NO_KEYCHAIN))
            } else {
                answer(request, R::Busy, None)
            });
        };
        let submitted = match request.body.as_ref() {
            Some(Body::SecretPut(v)) => store
                .put_named(
                    name.clone().unwrap_or_default(),
                    value,
                    v.replace == Some(true),
                )
                .map(Waiting::Put),
            Some(Body::SecretRemove(_)) => store
                .delete_named(name.clone().unwrap_or_default())
                .map(Waiting::Remove),
            Some(Body::SecretList(_)) => store.list_named().map(Waiting::List),
            _ => return Some(super::error(request, "invalid_request")),
        };
        match submitted {
            Ok(waiting) => {
                self.persons.named.push(Pending { request, waiting });
                None
            }
            Err(Refused::Busy) => Some(answer(request, R::Busy, None)),
            Err(Refused::Stopped) => Some(answer(request, R::Unavailable, Some(NO_KEYCHAIN))),
        }
    }

    /// Answers every named request whose worker reply arrived.
    pub(super) fn poll_named(&mut self) {
        let pending = std::mem::take(&mut self.persons.named);
        for mut item in pending {
            let done = match &mut item.waiting {
                Waiting::Put(ticket) => match ticket.poll() {
                    Polled::Pending => None,
                    Polled::Ready(r) => Some(self.named_put_done(r)),
                    Polled::Lost => Some(lost()),
                },
                Waiting::Remove(ticket) => match ticket.poll() {
                    Polled::Pending => None,
                    Polled::Ready(r) => Some(self.named_remove_done(r)),
                    Polled::Lost => Some(lost()),
                },
                Waiting::List(ticket) => match ticket.poll() {
                    Polled::Pending => None,
                    Polled::Ready(r) => Some(self.named_list_done(r)),
                    Polled::Lost => Some(lost()),
                },
            };
            match done {
                None => self.persons.named.push(item),
                Some(body) => self.out.push_back(reply(item.request, body)),
            }
        }
    }

    fn named_put_done(&self, result: NamedPutReply) -> Body {
        use pb::SecretResult as R;
        let (code, status, session): AuditReply = match &result {
            NamedPutReply::Stored => (StoreReply::Stored, None, None),
            NamedPutReply::Replaced => (StoreReply::Replaced, None, None),
            NamedPutReply::Exists => (StoreReply::Exists, None, None),
            NamedPutReply::Absent => (StoreReply::Absent, None, None),
            NamedPutReply::Full => (StoreReply::Full, None, None),
            NamedPutReply::Failed(failure) => failure_audit(failure),
        };
        self.audit_store(Event::SecretStoreNamedPut {
            reply: code,
            status,
            session,
        });
        let (result, status) = match result {
            NamedPutReply::Stored => (R::Stored, None),
            NamedPutReply::Replaced => (R::Replaced, None),
            NamedPutReply::Exists => (R::Exists, None),
            NamedPutReply::Absent => (R::Absent, None),
            NamedPutReply::Full => (R::Full, None),
            NamedPutReply::Failed(failure) => failure_result(&failure),
        };
        secret_reply(result, Vec::new(), status)
    }

    fn named_remove_done(&self, result: DeleteReply) -> Body {
        use pb::SecretResult as R;
        let (reply, status, session) = delete_audit(&result);
        self.audit_store(Event::SecretStoreNamedDelete {
            reply,
            status,
            session,
        });
        let (result, status) = match result {
            DeleteReply::Deleted => (R::Removed, None),
            DeleteReply::Absent => (R::Absent, None),
            DeleteReply::Failed(failure) => failure_result(&failure),
        };
        secret_reply(result, Vec::new(), status)
    }

    fn named_list_done(&self, result: NamedListReply) -> Body {
        use pb::SecretResult as R;
        match result {
            NamedListReply::Names(names) => {
                self.audit_store(Event::SecretStoreNamedList {
                    reply: StoreReply::Listed,
                    status: None,
                    session: None,
                    count: names.len() as u32,
                });
                secret_reply(R::Listed, names, None)
            }
            NamedListReply::Failed(failure) => {
                let (reply, status, session) = failure_audit(&failure);
                self.audit_store(Event::SecretStoreNamedList {
                    reply,
                    status,
                    session,
                    count: 0,
                });
                let (result, status) = failure_result(&failure);
                secret_reply(result, Vec::new(), status)
            }
        }
    }
}

fn secret_reply(result: pb::SecretResult, names: Vec<String>, status: Option<u32>) -> Body {
    Body::SecretReply(pb::SecretReply {
        result: Some(result as i32),
        names,
        status,
    })
}

/// The worker exited without replying.
fn lost() -> Body {
    secret_reply(pb::SecretResult::Unavailable, Vec::new(), Some(NO_KEYCHAIN))
}
