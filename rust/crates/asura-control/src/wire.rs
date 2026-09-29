use crate::ProtocolError;
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    U64,
    Bool,
    U32,
    Enum(&'static [i32]),
    String,
    Bytes,
    Message(usize),
}
pub(crate) struct Field {
    pub(crate) tag: u32,
    pub(crate) kind: Kind,
    pub(crate) repeated: bool,
    pub(crate) repeated_limit: usize,
    pub(crate) oneof: Option<usize>,
}
fn varint(bytes: &[u8], offset: &mut usize) -> Result<u64, ProtocolError> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*offset).ok_or(ProtocolError::MalformedWire)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(ProtocolError::MalformedWire);
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(ProtocolError::MalformedWire)
}
fn scalar(kind: Kind, value: u64) -> Result<(), ProtocolError> {
    match kind {
        Kind::U64 => Ok(()),
        Kind::Bool if value <= 1 => Ok(()),
        Kind::U32 if value <= u64::from(u32::MAX) => Ok(()),
        Kind::Enum(values) if i32::try_from(value).is_ok_and(|v| values.contains(&v)) => Ok(()),
        _ => Err(ProtocolError::MalformedWire),
    }
}
pub(crate) fn scan(
    bytes: &[u8],
    table: usize,
    depth: usize,
    tables: &[&[Field]],
) -> Result<(), ProtocolError> {
    if depth > 16 {
        return Err(ProtocolError::MalformedWire);
    }
    let fields = tables[table];
    let mut seen = vec![false; fields.len()];
    let mut oneofs = Vec::new();
    let mut repeated_mode = vec![0u8; fields.len()];
    let mut counts = vec![0usize; fields.len()];
    let mut offset = 0;
    while offset < bytes.len() {
        let key = varint(bytes, &mut offset)?;
        let tag = u32::try_from(key >> 3).map_err(|_| ProtocolError::MalformedWire)?;
        let wire = key & 7;
        let (index, field) = fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.tag == tag)
            .ok_or(ProtocolError::MalformedWire)?;
        if !field.repeated && seen[index] {
            return Err(ProtocolError::MalformedWire);
        }
        seen[index] = true;
        if let Some(oneof) = field.oneof {
            if oneofs.contains(&oneof) {
                return Err(ProtocolError::MalformedWire);
            }
            oneofs.push(oneof);
        }
        if field.repeated && !matches!(field.kind, Kind::Message(_)) {
            let mode = if wire == 2 { 2 } else { 1 };
            if repeated_mode[index] != 0 && (repeated_mode[index] != mode || mode == 2) {
                return Err(ProtocolError::MalformedWire);
            }
            repeated_mode[index] = mode;
        }
        let scalar_kind = matches!(
            field.kind,
            Kind::U64 | Kind::U32 | Kind::Bool | Kind::Enum(_)
        );
        if wire == 0 && scalar_kind {
            scalar(field.kind, varint(bytes, &mut offset)?)?;
            counts[index] += 1;
        } else if wire == 2 && (!scalar_kind || field.repeated) {
            let length = usize::try_from(varint(bytes, &mut offset)?)
                .map_err(|_| ProtocolError::MalformedWire)?;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or(ProtocolError::MalformedWire)?;
            let value = &bytes[offset..end];
            offset = end;
            match field.kind {
                Kind::Message(child) => {
                    counts[index] += 1;
                    if field.repeated && counts[index] > field.repeated_limit {
                        return Err(ProtocolError::MalformedWire);
                    }
                    scan(value, child, depth + 1, tables)?;
                }
                Kind::Bytes | Kind::String => (),
                _ => {
                    let mut at = 0;
                    while at < value.len() {
                        scalar(field.kind, varint(value, &mut at)?)?;
                        counts[index] += 1;
                        if counts[index] > 16 {
                            return Err(ProtocolError::MalformedWire);
                        }
                    }
                }
            }
        } else {
            return Err(ProtocolError::MalformedWire);
        }
        if counts[index] > 16 && field.repeated && !matches!(field.kind, Kind::Message(_)) {
            return Err(ProtocolError::MalformedWire);
        }
    }
    Ok(())
}
