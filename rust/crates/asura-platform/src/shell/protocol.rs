use super::ShellError;
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
    pub fn decode(b: &[u8], id: [u8; 16]) -> Result<Self, ShellError> {
        if b.len() != 32
            || &b[..4] != b"ASSH"
            || id == [0; 16]
            || b[8..24] != id
            || b[28..] != [0; 4]
            || b[7] > 1
            || b[5] > 9
        {
            return Err(ShellError::Protocol);
        }
        let n = Self {
            kind: b[4],
            reason: b[5],
            exit_kind: b[6],
            cleanup: b[7] == 1,
            value: u32::from_be_bytes(b[24..28].try_into().map_err(|_| ShellError::Protocol)?),
        };
        let exit = match n.exit_kind {
            0 => n.value == 0,
            1 => n.value <= 255,
            2 => (1..=127).contains(&n.value),
            _ => false,
        };
        let shape = match n.kind {
            1 => n.reason == 0 && n.exit_kind == 0 && !n.cleanup,
            2 => n.reason <= 4 && n.exit_kind != 0 && n.cleanup,
            3 => (5..=9).contains(&n.reason) && n.exit_kind == 0 && n.cleanup,
            4 => n.reason == 9 && !n.cleanup,
            _ => false,
        };
        if exit && shape {
            Ok(n)
        } else {
            Err(ShellError::Protocol)
        }
    }
}
