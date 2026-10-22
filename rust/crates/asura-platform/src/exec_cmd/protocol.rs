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
            2 => {
                (n.reason <= 4 || n.reason == REASON_ESCAPEE_TERMINATED)
                    && n.exit_kind != 0
                    && n.cleanup
            }
            3 => (5..=9).contains(&n.reason) && n.exit_kind == 0 && n.cleanup,
            4 => {
                (n.reason == REASON_CLEANUP_FAILED || n.reason == REASON_ESCAPEE_UNTERMINATED)
                    && n.exit_kind == 0
                    && !n.cleanup
            }
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
/// Kind 6: escapees were terminated during cleanup; value is their count.
pub(super) const KIND_ESCAPEE_REPORT: u8 = 6;
/// Kind 4 reason 9: cleanup did not settle by the 2.5 s report point.
pub(super) const REASON_CLEANUP_FAILED: u8 = 9;
/// Kind 2 reason 12: the gate passed only after escapees were terminated.
pub(super) const REASON_ESCAPEE_TERMINATED: u8 = 12;
/// Kind 4 reason 13: a descendant could not be verified gone.
pub(super) const REASON_ESCAPEE_UNTERMINATED: u8 = 13;
