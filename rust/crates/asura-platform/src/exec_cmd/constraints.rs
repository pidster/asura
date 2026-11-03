//! The `model-1` constraint set: the compiled effect envelope of a policy
//! decision, the guardian's profile input (security-policy.md, PL1
//! implementation contract, constraint set). The policy owner decides the
//! regions; this module owns their encoding and the Seatbelt rendering.
use super::ExecCmdError;
use super::guardian::{ancestor_literals, implicit_deny_rules, quote, state_rules};
use sha2::{Digest, Sha256};

/// The nine `model-1` template roots, in render order. Bit `i` of a root
/// set is root `i`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Root {
    Bin = 0,
    UsrBin = 1,
    Sbin = 2,
    UsrSbin = 3,
    UsrLib = 4,
    UsrShare = 5,
    System = 6,
    Project = 7,
    Scratch = 8,
}
impl Root {
    pub const ALL: [Self; 9] = [
        Self::Bin,
        Self::UsrBin,
        Self::Sbin,
        Self::UsrSbin,
        Self::UsrLib,
        Self::UsrShare,
        Self::System,
        Self::Project,
        Self::Scratch,
    ];
    /// The bit of this root in a root set.
    pub const fn bit(self) -> u16 {
        1 << self as u16
    }
    /// The fixed path of a system root; `None` for the project and scratch.
    pub const fn system_path(self) -> Option<&'static str> {
        Some(match self {
            Self::Bin => "/bin",
            Self::UsrBin => "/usr/bin",
            Self::Sbin => "/sbin",
            Self::UsrSbin => "/usr/sbin",
            Self::UsrLib => "/usr/lib",
            Self::UsrShare => "/usr/share",
            Self::System => "/System",
            Self::Project | Self::Scratch => return None,
        })
    }
}
const ALL_ROOTS: u16 = 0x1ff;
const WRITABLE: u16 = Root::Project.bit() | Root::Scratch.bit();
const NEEDED: u16 = Root::Bin.bit() | Root::Scratch.bit();

/// The allowed regions of one `model-1` job: roots it may read, execute
/// and map, and write, create and delete. A value always holds the base
/// needs: `/bin` and scratch in `read` and `exec`, scratch in `write`; only
/// the project root and scratch may be written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Constraints {
    read: u16,
    exec: u16,
    write: u16,
}
impl Constraints {
    /// A checked set; any other shape is `Invalid` (the policy compiler
    /// denies such a job `cannot_enforce` before this point).
    pub fn model1(read: u16, exec: u16, write: u16) -> Result<Self, ExecCmdError> {
        if read & !ALL_ROOTS != 0
            || exec & !ALL_ROOTS != 0
            || write & !WRITABLE != 0
            || read & NEEDED != NEEDED
            || exec & NEEDED != NEEDED
            || write & Root::Scratch.bit() == 0
        {
            return Err(ExecCmdError::Invalid);
        }
        Ok(Self { read, exec, write })
    }
    pub fn read(&self) -> u16 {
        self.read
    }
    pub fn exec(&self) -> u16 {
        self.exec
    }
    pub fn write(&self) -> u16 {
        self.write
    }
    /// `m1:<read>:<exec>:<write>`, three lowercase hex digits each.
    pub fn encoding(&self) -> String {
        format!("m1:{:03x}:{:03x}:{:03x}", self.read, self.exec, self.write)
    }
    /// The kind 34 constraint digest: SHA-256 of [`Self::encoding`].
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.encoding().as_bytes()).into()
    }
    /// The guardian argv token: the encoding and the expected profile digest.
    pub(super) fn token(&self, profile_digest: &[u8; 32]) -> String {
        let mut token = self.encoding();
        token.push(':');
        for byte in profile_digest {
            token.push_str(&format!("{byte:02x}"));
        }
        token
    }
    /// The exact token form, or `Invalid`.
    pub(super) fn parse_token(token: &str) -> Result<(Self, [u8; 32]), ExecCmdError> {
        let parts: Vec<&str> = token.split(':').collect();
        let hex = |s: &str, len: usize| {
            s.len() == len
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let [tag, read, exec, write, digest] = parts.as_slice() else {
            return Err(ExecCmdError::Invalid);
        };
        if *tag != "m1" || ![read, exec, write].iter().all(|s| hex(s, 3)) || !hex(digest, 64) {
            return Err(ExecCmdError::Invalid);
        }
        let mask = |s: &str| u16::from_str_radix(s, 16).map_err(|_| ExecCmdError::Invalid);
        let value = Self::model1(mask(read)?, mask(exec)?, mask(write)?)?;
        let mut expected = [0u8; 32];
        for (i, byte) in expected.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16)
                .map_err(|_| ExecCmdError::Invalid)?;
        }
        Ok((value, expected))
    }
}

/// The SHA-256 of a rendered profile.
pub fn profile_digest(profile: &str) -> [u8; 32] {
    Sha256::digest(profile.as_bytes()).into()
}

/// The `model-1` Seatbelt profile for `constraints` and the pinned paths.
/// Rule order is today's: the combined read and exec rule, read-only and
/// exec-only rules, the ancestor metadata of every read or exec root, the
/// write rule, the runtime set, the state rules and the implicit-execution
/// deny rules. A rule with no roots is left out. Command text never enters.
pub fn render_model1(
    constraints: &Constraints,
    project: &str,
    scratch: &str,
    asura: &str,
) -> Result<String, ExecCmdError> {
    let path = |root: Root| {
        root.system_path().unwrap_or(match root {
            Root::Project => project,
            _ => scratch,
        })
    };
    let roots = |mask: u16| -> Vec<&str> {
        Root::ALL
            .into_iter()
            .filter(|root| mask & root.bit() != 0)
            .map(path)
            .collect()
    };
    let filters = |paths: &[&str]| -> Result<String, ExecCmdError> {
        let mut text = String::new();
        for p in paths {
            text.push_str(&format!(" (subpath {})", quote(p)?));
        }
        Ok(text)
    };
    let both = roots(constraints.read & constraints.exec);
    let read_only = roots(constraints.read & !constraints.exec);
    let exec_only = roots(constraints.exec & !constraints.read);
    let mut rules = String::new();
    if !both.is_empty() {
        rules.push_str(&format!(
            "(allow process-exec file-map-executable file-read*{})",
            filters(&both)?
        ));
    }
    if !read_only.is_empty() {
        rules.push_str(&format!("(allow file-read*{})", filters(&read_only)?));
    }
    if !exec_only.is_empty() {
        rules.push_str(&format!(
            "(allow process-exec file-map-executable{})",
            filters(&exec_only)?
        ));
    }
    let metadata = ancestor_literals(&roots(constraints.read | constraints.exec))?;
    let write = filters(&roots(constraints.write))?;
    let state = state_rules(asura, scratch)?;
    let deny = implicit_deny_rules(project)?;
    Ok(format!(
        "(version 1)(deny default)(allow file-read* file-test-existence (literal \"/\"))(allow file-read* (literal \"/private/var/select/sh\"))(allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))(allow process-fork){rules}(allow file-read-metadata{metadata})(allow file-write*{write})(allow sysctl-read)(allow file-read* file-write* (literal \"/dev/null\"))(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\")){state}{deny}"
    ))
}

/// The `model-1` template: every root readable and executable, the project
/// root and scratch writable. Tests only; production sets come from the
/// policy owner.
#[cfg(any(test, feature = "test-support"))]
pub fn template_for_test() -> Constraints {
    Constraints::model1(ALL_ROOTS, ALL_ROOTS, WRITABLE).expect("template")
}

/// The frozen `model-1` generator of commit 35cc82b, for the PL1 byte-equal
/// golden tests (PL-U7, PL-I2). Never used to start a job.
#[cfg(any(test, feature = "test-support"))]
pub fn legacy_model1_for_test(
    project: &str,
    scratch: &str,
    asura: &str,
) -> Result<String, ExecCmdError> {
    let roots = [
        "/bin",
        "/usr/bin",
        "/sbin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/share",
        "/System",
        project,
        scratch,
    ];
    let mut filters = String::new();
    for root in roots {
        filters.push_str(&format!(" (subpath {})", quote(root)?));
    }
    let metadata = ancestor_literals(&roots)?;
    let state = state_rules(asura, scratch)?;
    let deny = implicit_deny_rules(project)?;
    Ok(format!(
        "(version 1)(deny default)(allow file-read* file-test-existence (literal \"/\"))(allow file-read* (literal \"/private/var/select/sh\"))(allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))(allow process-fork)(allow process-exec file-map-executable file-read*{filters})(allow file-read-metadata{metadata})(allow file-write* (subpath {}) (subpath {}))(allow sysctl-read)(allow file-read* file-write* (literal \"/dev/null\"))(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\")){state}{deny}",
        quote(project)?,
        quote(scratch)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GOLDEN: &str = include_str!("../../tests/fixtures/model-1-profile-35cc82b.txt");
    const PATHS: [(&str, &str, &str); 2] = [
        (
            "/Users/person/src/project",
            "/Users/person/.asura/tmp/exec-cmd-00112233445566778899aabbccddeeff",
            "/Users/person/.asura",
        ),
        (
            "/private/tmp/a b/pro\"j.c+(x)\\y",
            "/private/tmp/h o/.asura/tmp/exec-cmd-0f",
            "/private/tmp/h o/.asura",
        ),
    ];

    /// PL-U7 (guardian part): the template constraint set renders today's
    /// profile byte for byte; the text was captured from the generator of
    /// commit 35cc82b before PL1 changed it.
    #[test]
    fn pl_u7_template_renders_the_35cc82b_profile_byte_for_byte() {
        let golden: Vec<&str> = GOLDEN.lines().collect();
        assert_eq!(golden.len(), PATHS.len());
        for ((project, scratch, asura), expected) in PATHS.into_iter().zip(golden) {
            let text = render_model1(&template_for_test(), project, scratch, asura).unwrap();
            assert_eq!(text, expected);
            assert_eq!(
                legacy_model1_for_test(project, scratch, asura).unwrap(),
                expected
            );
        }
    }
    /// A dropped root leaves its rule; read-only and exec-only roots get
    /// their own rules; the fixed deny rules stay.
    #[test]
    fn narrower_sets_render_narrower_rules() {
        let all = ALL_ROOTS;
        let c = Constraints::model1(all, all & !Root::UsrShare.bit(), Root::Scratch.bit()).unwrap();
        let text = render_model1(&c, "/p", "/h/.asura/tmp/s", "/h/.asura").unwrap();
        assert!(text.contains("(allow file-read* (subpath \"/usr/share\"))"));
        assert!(!text.contains("file-read* (subpath \"/usr/lib\") (subpath \"/usr/share\")"));
        assert!(
            text.contains("(allow file-write* (subpath \"/h/.asura/tmp/s\"))(allow sysctl-read)")
        );
        assert!(text.contains("(deny file-write* (subpath \"/p/.git/hooks\")"));
        assert!(text.contains(
            "(deny file-read* file-write* process-exec file-map-executable (subpath \"/h/.asura\"))"
        ));
        let exec_only = Constraints::model1(all & !Root::System.bit(), all, WRITABLE).unwrap();
        let text = render_model1(&exec_only, "/p", "/h/.asura/tmp/s", "/h/.asura").unwrap();
        assert!(text.contains("(allow process-exec file-map-executable (subpath \"/System\"))"));
    }
    #[test]
    fn shapes_outside_the_contract_are_invalid() {
        let all = ALL_ROOTS;
        for (r, x, w) in [
            (all | 0x200, all, WRITABLE),
            (all, all, WRITABLE | Root::Bin.bit()),
            (all & !Root::Bin.bit(), all, WRITABLE),
            (all, all & !Root::Scratch.bit(), WRITABLE),
            (all, all, Root::Project.bit()),
        ] {
            assert_eq!(Constraints::model1(r, x, w), Err(ExecCmdError::Invalid));
        }
    }
    #[test]
    fn token_round_trips_and_refuses_every_other_form() {
        let c = template_for_test();
        assert_eq!(c.encoding(), "m1:1ff:1ff:180");
        let digest = [0xab; 32];
        let token = c.token(&digest);
        assert_eq!(Constraints::parse_token(&token), Ok((c, digest)));
        for bad in [
            "-",
            "",
            "m1:1ff:1ff:180",
            &token.replace("m1", "m2"),
            &token.to_uppercase(),
            &format!("{token}0"),
            &token.replacen("1ff", "fff", 1),
            &token.replacen(":180", ":181", 1),
            &format!("{token}:"),
        ] {
            assert_eq!(
                Constraints::parse_token(bad),
                Err(ExecCmdError::Invalid),
                "{bad}"
            );
        }
        assert_eq!(c.digest(), profile_digest("m1:1ff:1ff:180"));
    }
}
