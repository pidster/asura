use super::ExecCmdError;
#[derive(Clone, Copy, Debug)]
pub(super) struct Notice {
    pub kind: u8,
    pub reason: u8,
    pub exit_kind: u8,
    pub cleanup: bool,
    pub value: u32,
}
impl Notice {
    pub fn encode(self, id: [u8; 16]) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(b"ASSH");
        b[4] = self.kind;
        b[5] = self.reason;
        b[6] = self.exit_kind;
        b[7] = u8::from(self.cleanup);
        b[8..24].copy_from_slice(&id);
        b[24..28].copy_from_slice(&self.value.to_be_bytes());
        b
    }
    /// Shape rules (exec-cmd-tool.md notice contract; escapee-control.md
    /// notice extensions). Order rules belong to the reader.
    pub fn decode(b: &[u8], id: [u8; 16]) -> Result<Self, ExecCmdError> {
        if b.len() != 32
            || &b[..4] != b"ASSH"
            || id == [0; 16]
            || b[8..24] != id
            || b[28..] != [0; 4]
            || b[7] > 1
        {
            return Err(ExecCmdError::Protocol);
        }
        let n = Self {
            kind: b[4],
            reason: b[5],
            exit_kind: b[6],
            cleanup: b[7] == 1,
            value: u32::from_be_bytes(b[24..28].try_into().map_err(|_| ExecCmdError::Protocol)?),
        };
        let exit = match (n.kind, n.exit_kind) {
            // Kind 6 carries the escapee count, never an exit.
            (KIND_ESCAPEE_REPORT, 0) => n.value >= 1,
            (KIND_ESCAPEE_REPORT, _) => false,
            (_, 0) => n.value == 0,
            (_, 1) => n.value <= 255,
            (_, 2) => (1..=127).contains(&n.value),
            _ => false,
        };
        let shape = match n.kind {
            1 => n.reason == 0 && n.exit_kind == 0 && !n.cleanup,
            2 => SETTLED_REASONS.contains(&n.reason) && n.exit_kind != 0 && n.cleanup,
            3 => (5..=9).contains(&n.reason) && n.exit_kind == 0 && n.cleanup,
            4 => {
                (n.reason == REASON_CLEANUP_FAILED || n.reason == REASON_ESCAPEE_UNTERMINATED)
                    && n.exit_kind == 0
                    && !n.cleanup
            }
            KIND_LEADER_EXITED => n.reason == 0 && n.exit_kind != 0 && !n.cleanup,
            KIND_ESCAPEE_REPORT => n.reason == 0 && !n.cleanup,
            _ => false,
        };
        if exit && shape {
            Ok(n)
        } else {
            Err(ExecCmdError::Protocol)
        }
    }
}
/// Kind 5: the leader exited; exit kind and value are its `waitid` facts
/// (tool-job-controller.md, guardian contract additions).
pub(super) const KIND_LEADER_EXITED: u8 = 5;
/// Kind 2 reasons: 0 leader exit, 2 `cancel` (lease `T`), 4 `lease_ended`,
/// 11 `kill` (lease `K`), 12 `escapee_terminated`, 14
/// `escapees_left_running` and 15 `escapees_hold_output`. Reasons 1, 3 and
/// 10 are unassigned. Which reasons follow kind 6 depends on the profile;
/// that is an order rule for the reader.
pub(super) const SETTLED_REASONS: [u8; 7] = [
    0,
    REASON_CANCEL,
    REASON_LEASE_ENDED,
    REASON_KILL,
    REASON_ESCAPEE_TERMINATED,
    REASON_ESCAPEES_LEFT_RUNNING,
    REASON_ESCAPEES_HOLD_OUTPUT,
];
/// Kind 2 reason 2: the first stop came from lease byte `T`.
pub(super) const REASON_CANCEL: u8 = 2;
/// Kind 2 reason 4: the first stop came from lease EOF or a lease fault.
pub(super) const REASON_LEASE_ENDED: u8 = 4;
/// Kind 2 reason 11: lease byte `K` killed the group; replaces reason 2.
pub(super) const REASON_KILL: u8 = 11;
/// Kind 6: escapees were terminated during cleanup; value is their count.
pub(super) const KIND_ESCAPEE_REPORT: u8 = 6;
/// Kind 4 reason 9: cleanup did not settle by the 2.5 s report point.
pub(super) const REASON_CLEANUP_FAILED: u8 = 9;
/// Kind 2 reason 12: the gate passed only after escapees were terminated.
pub(super) const REASON_ESCAPEE_TERMINATED: u8 = 12;
/// Kind 2 reason 14, `person-outside-1` only, after kind 6: survivors were
/// left running and none holds a capture pipe (escapee-control.md).
pub(super) const REASON_ESCAPEES_LEFT_RUNNING: u8 = 14;
/// Kind 2 reason 15, `person-outside-1` only, after kind 6: at least one
/// survivor holds a capture pipe, so capture continues after the settled
/// notice (OD2).
pub(super) const REASON_ESCAPEES_HOLD_OUTPUT: u8 = 15;
/// Kind 4 reason 13: a descendant could not be verified gone.
pub(super) const REASON_ESCAPEE_UNTERMINATED: u8 = 13;
/// Lease byte `T` on FD 3: the first stop, TERM now and KILL after the
/// profile's grace.
pub(super) const LEASE_TERM: u8 = b'T';
/// Lease byte `K` on FD 3: KILL the owned group now.
pub(super) const LEASE_KILL: u8 = b'K';

/// The guardian profile, passed as an ASCII token in the private argv
/// (tool-job-controller.md, guardian contract additions). It selects the
/// sandbox, the child environment and the TERM-to-KILL grace
/// (person-command-mode.md, platform changes). The person profiles' data
/// travels in a [`super::PersonSetup`], never in the token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Profile {
    /// `model-1`: the exec_cmd Seatbelt profile, 500 ms grace.
    Model1,
    /// `person-1`: the person Seatbelt profile for a `!` command, 2 s grace.
    Person1,
    /// `person-outside-1`: no Seatbelt profile, for a `!!` command, 2 s
    /// grace; the group is not stopped at shell exit (PQ3).
    PersonOutside1,
}
impl Profile {
    /// The argv token.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Model1 => "model-1",
            Self::Person1 => "person-1",
            Self::PersonOutside1 => "person-outside-1",
        }
    }
    /// The exact token, or `None` for any other text.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "model-1" => Some(Self::Model1),
            "person-1" => Some(Self::Person1),
            "person-outside-1" => Some(Self::PersonOutside1),
            _ => None,
        }
    }
    /// Whether the command runs under `sandbox-exec`.
    pub const fn sandboxed(self) -> bool {
        !matches!(self, Self::PersonOutside1)
    }
    /// Whether the guardian reads a person environment descriptor (FD 11).
    pub const fn person(self) -> bool {
        !matches!(self, Self::Model1)
    }
    /// TERM-to-KILL grace after the first stop, in nanoseconds. A wind-down
    /// bound after a closure; it ends nothing by itself.
    pub(super) const fn grace_ns(self) -> u64 {
        match self {
            Self::Model1 => 500_000_000,
            // PC8.
            Self::Person1 | Self::PersonOutside1 => 2_000_000_000,
        }
    }
}
