//! Named secrets (`docs/designs/named-secrets.md`): the secret name rule and
//! the named reference parser and resolver of person commands (AD-10 for
//! named references). Pure computation over at most one command; no value
//! ever passes through this module.

/// The variable prefix every secret variable and reference shares.
pub const PREFIX: &str = "ASURA_SECRET_";
/// The longest secret name: `ASURA_SECRET_<NAME>` stays at most 61 bytes.
pub const MAX_NAME_BYTES: usize = 48;
/// Reserved: `ASURA_SECRET_COMMAND` is the whole-command variable.
pub const RESERVED_NAME: &str = "COMMAND";
/// Distinct secret variables one command carries, numbered and named.
pub const MAX_VARIABLES: usize = super::rewrite::MAX_SECRET_VARIABLES;
/// The longest command after the references are replaced.
pub const MAX_COMMAND_BYTES: usize = super::rewrite::MAX_REWRITTEN_COMMAND_BYTES;

/// A secret name: 1–48 bytes of `[A-Za-z_][A-Za-z0-9_]*`, not `COMMAND`.
pub fn name_valid(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_NAME_BYTES).contains(&bytes.len())
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        && name != RESERVED_NAME
}

/// The canonical reference text of `name`: `${ASURA_SECRET_<NAME>}`.
pub fn reference_text(name: &str) -> String {
    format!("${{{PREFIX}{name}}}")
}

/// One named reference in a command: its byte range and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub start: usize,
    pub end: usize,
    pub name: String,
}

/// Why a command's references cannot be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceError {
    /// An operator form such as `${ASURA_SECRET_X:-y}`, or a name that is
    /// not a valid secret name (over 48 bytes, or a leading digit with
    /// other bytes): `secret_reference_invalid`.
    Invalid,
    /// More than 16 variables, or a command over 8,192 bytes after the
    /// replacement: `too_many_secrets`.
    TooMany,
}

fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every named reference in `text`, in order. `$ASURA_SECRET_<digits>` and
/// `ASURA_SECRET_COMMAND` are left to the placeholder refusal; a `$` with no
/// name byte after the prefix is ordinary text. Quoting is ignored: inside
/// single quotes the replaced text stays literal and exposes no value.
pub fn references(text: &str) -> Result<Vec<Reference>, ReferenceError> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let braced = rest.starts_with(b"${");
        let open = if braced {
            2
        } else if rest.starts_with(b"$") {
            1
        } else {
            at += 1;
            continue;
        };
        if !rest[open..].starts_with(PREFIX.as_bytes()) {
            at += 1;
            continue;
        }
        let name_start = open + PREFIX.len();
        let run = rest[name_start..]
            .iter()
            .take_while(|b| name_byte(**b))
            .count();
        let name = &rest[name_start..name_start + run];
        if run == 0 || name.iter().all(u8::is_ascii_digit) || name == RESERVED_NAME.as_bytes() {
            at += 1;
            continue;
        }
        let mut end = name_start + run;
        if braced {
            if rest.get(end) != Some(&b'}') {
                return Err(ReferenceError::Invalid);
            }
            end += 1;
        }
        // The run is ASCII name bytes, so it is valid UTF-8.
        let name = std::str::from_utf8(name).map_err(|_| ReferenceError::Invalid)?;
        if !name_valid(name) {
            return Err(ReferenceError::Invalid);
        }
        found.push(Reference {
            start: at,
            end: at + end,
            name: name.to_owned(),
        });
        at += end;
    }
    Ok(found)
}

/// Whether `bytes` holds a named reference (a rewrite value that does must
/// not be delivered as text).
pub fn holds_reference(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .is_some_and(|text| !matches!(references(text), Ok(r) if r.is_empty()))
}

/// A command with its named references replaced by numbered variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub command: String,
    /// `(number, name)` in increasing number: distinct names by first
    /// appearance, numbered from the first free number.
    pub names: Vec<(u8, String)>,
}

/// Replaces each named reference in `command` with `${ASURA_SECRET_<n>}`.
/// `numbered` is the count of the rewrite's own variables; the names get
/// `numbered + 1` onward.
pub fn resolve(command: &str, numbered: usize) -> Result<Resolved, ReferenceError> {
    let references = references(command)?;
    let mut names: Vec<(u8, String)> = Vec::new();
    let mut out = String::with_capacity(command.len());
    let mut cursor = 0;
    for reference in &references {
        let number = match names.iter().find(|(_, n)| *n == reference.name) {
            Some((number, _)) => *number,
            None => {
                let next = numbered + names.len() + 1;
                if next > MAX_VARIABLES {
                    return Err(ReferenceError::TooMany);
                }
                names.push((next as u8, reference.name.clone()));
                next as u8
            }
        };
        out.push_str(&command[cursor..reference.start]);
        out.push_str(&format!("${{{PREFIX}{number}}}"));
        cursor = reference.end;
    }
    out.push_str(&command[cursor..]);
    if out.len() > MAX_COMMAND_BYTES {
        return Err(ReferenceError::TooMany);
    }
    Ok(Resolved {
        command: out,
        names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NS-U1: the secret name rule.
    #[test]
    fn ns_u1_name_rule() {
        assert!(name_valid("A"));
        assert!(name_valid("_x"));
        assert!(name_valid("GITHUB_TOKEN"));
        assert!(name_valid(&"A".repeat(48)));
        for bad in ["", "1A", "A-B", "COMMAND", "A B", "É"] {
            assert!(!name_valid(bad), "{bad}");
        }
        assert!(!name_valid(&"A".repeat(49)));
        assert_eq!(reference_text("X"), "${ASURA_SECRET_X}");
    }

    /// NS-U2: the reference parser.
    #[test]
    fn ns_u2_references() {
        let text = r#"gh -H "$ASURA_SECRET_GITHUB_TOKEN" '${ASURA_SECRET_B}' $ASURA_SECRET_1 ${ASURA_SECRET_2} $ASURA_SECRET_COMMAND $ASURA_SECRET_ x"#;
        let found = references(text).unwrap();
        let names: Vec<&str> = found.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["GITHUB_TOKEN", "B"]);
        assert_eq!(
            &text[found[0].start..found[0].end],
            "$ASURA_SECRET_GITHUB_TOKEN"
        );
        assert_eq!(&text[found[1].start..found[1].end], "${ASURA_SECRET_B}");
        // The longest name run, as the shell reads it.
        let run = references("$ASURA_SECRET_AB-c").unwrap();
        assert_eq!(run[0].name, "AB");
        for bad in [
            "${ASURA_SECRET_X:-y}",
            "${ASURA_SECRET_X",
            "$ASURA_SECRET_1ABC",
        ] {
            assert_eq!(references(bad), Err(ReferenceError::Invalid), "{bad}");
        }
        let long = format!("$ASURA_SECRET_{}", "A".repeat(49));
        assert_eq!(references(&long), Err(ReferenceError::Invalid));
        assert!(holds_reference(b"$ASURA_SECRET_X"));
        assert!(!holds_reference(b"$ASURA_SECRET_1"));
        assert!(!holds_reference(b"plain"));
    }

    /// NS-U3: the resolver numbers names after the rewrite's variables.
    #[test]
    fn ns_u3_resolve() {
        let resolved = resolve(
            r#"a "${ASURA_SECRET_1}" $ASURA_SECRET_X ${ASURA_SECRET_Y} "$ASURA_SECRET_X""#,
            2,
        )
        .unwrap();
        assert_eq!(
            resolved.command,
            r#"a "${ASURA_SECRET_1}" ${ASURA_SECRET_3} ${ASURA_SECRET_4} "${ASURA_SECRET_3}""#
        );
        assert_eq!(
            resolved.names,
            vec![(3, "X".to_owned()), (4, "Y".to_owned())]
        );
        assert_eq!(
            resolve("$ASURA_SECRET_X $ASURA_SECRET_Y", 15),
            Err(ReferenceError::TooMany)
        );
        let long = format!("{} $ASURA_SECRET_A", "x".repeat(8_180));
        assert_eq!(resolve(&long, 0), Err(ReferenceError::TooMany));
        let none = resolve("echo hi", 3).unwrap();
        assert_eq!((none.command.as_str(), none.names.len()), ("echo hi", 0));
    }
}
