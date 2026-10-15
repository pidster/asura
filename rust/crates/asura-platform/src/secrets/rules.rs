//! Rule-set revision 1: Wisp's `SecretScanner` secret rules at commit 4305d2c,
//! ported under the construct rewrites G1–G3, B1, L1, A1, P1 and L2 of
//! `docs/designs/secret-detection.md`. Pure computation; no I/O.
use super::SecretKind;
use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use sha2::{Digest, Sha256};

pub const RULE_SET_REVISION: u32 = 1;
/// SHA-256 over the canonical rule table (see `canonical_table`), recorded at
/// revision 1. A rule change must change the revision and this value.
pub const RULE_SET_DIGEST: &str =
    "54cd1df419cddec67ea8ade827ba37fb6219993cb6be4eb0464395570f8985a9";
/// G3: compiled-size limit per expression.
pub(crate) const EXPRESSION_SIZE_LIMIT: usize = 1 << 20;
/// G3: lazy DFA cache limit per expression.
pub(crate) const DFA_CACHE_LIMIT: usize = 256 << 10;

/// A check applied to a match after the expression accepts it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Predicate {
    /// B1: the BEGIN line opens a block that the PEM state machine closes.
    PemBlock,
    NotPlaceholder,
    NotCode,
    ReadsAsCredential,
    /// L1: the byte before the credential name is absent or not `[A-Za-z0-9:]`.
    /// Encoded as the consuming prefix `(?:^|[^A-Za-z0-9:])`.
    NameBoundary,
    /// A1: after the closing quote and ASCII blanks, no `+` or `[`.
    NotContinued,
    /// P1: the byte after the value is not `(` or `[`.
    NotCallOrIndex,
    /// L2: the name is not 1–16 blanks after an ASCII letter or digit.
    KeyPosition,
    /// L2: the name is 1–16 blanks after an ASCII letter or digit.
    SentencePosition,
}
impl Predicate {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::PemBlock => "pem-block",
            Self::NotPlaceholder => "not-placeholder",
            Self::NotCode => "not-code",
            Self::ReadsAsCredential => "reads-as-credential",
            Self::NameBoundary => "name-boundary",
            Self::NotContinued => "not-continued",
            Self::NotCallOrIndex => "not-call-or-index",
            Self::KeyPosition => "key-position",
            Self::SentencePosition => "sentence-position",
        }
    }
    /// Structural predicates reject the match position: the search restarts one
    /// byte after the name start, as the Swift lookaround would.
    fn structural(self) -> bool {
        matches!(
            self,
            Self::NotContinued | Self::NotCallOrIndex | Self::KeyPosition | Self::SentencePosition
        )
    }
}

pub(crate) struct Rule {
    pub id: &'static str,
    pub kind: SecretKind,
    pub pattern: String,
    pub group: usize,
    pub predicates: &'static [Predicate],
    pub expression: usize,
}
impl Rule {
    fn has(&self, predicate: Predicate) -> bool {
        self.predicates.contains(&predicate)
    }
}

/// L1 prefix, then Wisp's `credentialName` without its lookbehind.
const NAME: &str = concat!(
    r"(?:^|[^A-Za-z0-9:])",
    r"(?:(?:[A-Za-z0-9]+[_.\-])+(?:pass|pw)|(?:[A-Za-z0-9]+[_.\-])*(?:password|passwd",
    r"|passphrase|pwd|secret(?:[_\-]?key)?|token|api[_\-]?key|access[_\-]?key|private[_\-]?key|encryption[_\-]?key",
    r"|signing[_\-]?key|auth[_\-]?key|credentials?))(?:[_.\-](?:hash|digest|base))?"
);
/// P1: Wisp's possessive `unquotedValue` as a greedy run; the lookahead is a predicate.
const UNQUOTED_VALUE: &str = r##"([^\s"'`,;{}()\[\]<>]{4,})"##;

use Predicate::{
    KeyPosition as KP, NameBoundary as L1, NotCallOrIndex as P1, NotCode as NC, NotContinued as A1,
    NotPlaceholder as NP, PemBlock as PEM, ReadsAsCredential as RC, SentencePosition as SP,
};

/// ID, kind, pattern (`None` reuses the previous expression), value group
/// and predicates.
type TableRow = (
    &'static str,
    SecretKind,
    Option<String>,
    usize,
    &'static [Predicate],
);

/// The rule table in Wisp's order. `None` as pattern reuses the previous
/// rule's expression (L2: SR-35 and SR-36 share one).
fn table() -> Vec<TableRow> {
    use SecretKind::*;
    let s = |p: &str| Some(p.to_string());
    vec![
        (
            "SR-01",
            PrivateKey,
            s(r"-----BEGIN ([A-Z ]*)PRIVATE KEY( BLOCK|)-----"),
            0,
            &[PEM],
        ),
        (
            "SR-02",
            PrivateKey,
            s(r"\b(?:MII[A-Za-z0-9+/]{2,3}IBA(?:AK|DAN)|b3BlbnNzaC1rZXktdjE)[A-Za-z0-9+/=]{20,}"),
            0,
            &[],
        ),
        (
            "SR-03",
            AwsAccessKey,
            s(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            0,
            &[NP],
        ),
        (
            "SR-04",
            AwsSecretKey,
            s(
                r#"(?i)aws[a-z0-9_\-]{0,20}(?:secret|private)[a-z0-9_\-]{0,20}["']?\s*[:=]\s*["']?([A-Za-z0-9/+]{40})\b"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-05",
            GithubToken,
            s(r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{50,})\b"),
            0,
            &[NP],
        ),
        (
            "SR-06",
            GitlabToken,
            s(r"\bgl(?:pat|dt|ptt|rt|cbt|soat|ft|imt)-[A-Za-z0-9_\-]{20,}"),
            0,
            &[],
        ),
        (
            "SR-07",
            PypiToken,
            s(r"\bpypi-AgE[A-Za-z0-9_\-]{16,}"),
            0,
            &[],
        ),
        (
            "SR-08",
            DockerToken,
            s(r"\bdckr_pat_[A-Za-z0-9_\-]{20,}"),
            0,
            &[],
        ),
        ("SR-09", NpmToken, s(r"\bnpm_[A-Za-z0-9]{30,}\b"), 0, &[]),
        (
            "SR-10",
            BitbucketToken,
            s(r"\bATBB[A-Za-z0-9]{24,}"),
            0,
            &[],
        ),
        (
            "SR-11",
            VaultToken,
            s(r"\b(?:hv[sb]\.[A-Za-z0-9_\-]{24,}|s\.[A-Za-z0-9]{24,}\b)"),
            0,
            &[],
        ),
        (
            "SR-12",
            FlyToken,
            s(r"\bFlyV1 fm[12]_[A-Za-z0-9_+/=\-]{20,}"),
            0,
            &[],
        ),
        (
            "SR-13",
            WebhookSecret,
            s(r"\bwhsec_[A-Za-z0-9+/=]{24,}"),
            0,
            &[],
        ),
        ("SR-14", MailgunKey, s(r"\bkey-[0-9a-f]{32}\b"), 0, &[]),
        (
            "SR-15",
            GoogleOauth,
            s(
                r"\b(?:GOCSPX-[A-Za-z0-9_\-]{20,}|1//0[A-Za-z0-9_\-]{20,}|ya29\.[A-Za-z0-9_\-]{20,})",
            ),
            0,
            &[],
        ),
        (
            "SR-16",
            AzureKey,
            s(r"(?i)\b(?:AccountKey|SharedAccessKey)=([A-Za-z0-9+/]{40,}={0,2})"),
            1,
            &[],
        ),
        (
            "SR-17",
            SlackToken,
            s(r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
            0,
            &[],
        ),
        (
            "SR-18",
            SlackWebhook,
            s(r"https://hooks\.slack\.com/services/[A-Za-z0-9/]{20,}"),
            0,
            &[],
        ),
        (
            "SR-19",
            ChatWebhook,
            s(
                r"https://(?:(?:ptb\.|canary\.)?discord(?:app)?\.com/api/webhooks/\d+/[A-Za-z0-9_\-]{20,}|[a-z0-9.\-]*(?:webhook\.office|outlook\.office)\.com/webhook[b2]?/\S{20,})",
            ),
            0,
            &[],
        ),
        (
            "SR-20",
            StripeKey,
            s(r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}\b"),
            0,
            &[],
        ),
        (
            "SR-21",
            AnthropicKey,
            s(r"\bsk-ant-[A-Za-z0-9_\-]{20,}"),
            0,
            &[],
        ),
        (
            "SR-22",
            OpenaiKey,
            s(r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{20,}"),
            0,
            &[],
        ),
        (
            "SR-23",
            GoogleApiKey,
            s(r"\bAIza[0-9A-Za-z_\-]{35}\b"),
            0,
            &[],
        ),
        (
            "SR-24",
            Jwt,
            s(r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
            0,
            &[],
        ),
        (
            "SR-25",
            UrlPassword,
            s(r"\b[a-z][a-z0-9+.\-]*://[^/\s:@]*:([^/\s:@]{3,})@"),
            1,
            &[NP],
        ),
        (
            "SR-26",
            UrlSecret,
            s(
                r##"(?i)[?&](?:sig|signature|x-goog-signature|x-amz-signature|token|access_token|api_key|apikey|key|secret|password|code)=([^&\s"'#]{6,})"##,
            ),
            1,
            &[NP],
        ),
        (
            "SR-27",
            AuthHeader,
            s(
                r#"(?i)\b(?:proxy-)?authorization["']?\s*[:=,]\s*["']?(?:bearer|basic|token|digest|apikey|sso-key)\s+([A-Za-z0-9._~+/:\-]{6,}=*)"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-28",
            AuthHeader,
            s(r"\bBearer\s+([A-Za-z0-9._~+/\-]{20,}=*)"),
            1,
            &[NP],
        ),
        (
            "SR-29",
            SessionCookie,
            s(
                r#"(?i)\b(?:set-)?cookie\s*:[^\n]*?\b[A-Za-z0-9_\-]*(?:sess(?:ion)?(?:id)?|sid|token|auth|remember[A-Za-z0-9_]*)=([^;\s"']{8,})"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-30",
            PasswordHash,
            s(
                r"\$(?:2[abxy]?\$\d{2}\$[./A-Za-z0-9]{50,}|(?:1|5|6|apr1|y)\$[^$\s:]{1,64}\$[./A-Za-z0-9]{20,})",
            ),
            0,
            &[],
        ),
        (
            "SR-31",
            AssignedSecret,
            s(
                r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|pwd|credential|auth[_-]?key)[a-z0-9_]*["']?\s*[:=]\s*["']([^"'\s]{8,})["']"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-32",
            EnvSecret,
            s(
                r##"(?m)^\s*(?:export\s+)?[A-Z0-9_]*(?:KEY|SECRET|TOKEN|PASSWORD|PASSWD)[A-Z0-9_]*\s*=\s*([^\s#"']{8,})"##,
            ),
            1,
            &[NP, NC],
        ),
        (
            "SR-33",
            AssignedSecret,
            Some(format!(
                r#"(?i){NAME}["']?\s*[:=]\s*["']([^"'\n]{{4,}})["']"#
            )),
            1,
            &[L1, A1, NP],
        ),
        (
            "SR-34",
            AssignedSecret,
            Some(format!(r#"(?i){NAME}["']?\s*=\s*{UNQUOTED_VALUE}"#)),
            1,
            &[L1, P1, NP, NC],
        ),
        (
            "SR-35",
            AssignedSecret,
            Some(format!(r#"(?i){NAME}["']?\s*:\s*{UNQUOTED_VALUE}"#)),
            1,
            &[L1, P1, KP, NP, NC],
        ),
        ("SR-36", AssignedSecret, None, 1, &[L1, P1, SP, NP, NC, RC]),
        (
            "SR-37",
            CliPassword,
            s(
                r#"(?i)--(?:password|passwd|pass|token|api-key|secret|client-secret)(?:=|\s+)['"]?([^\s'"]{3,})"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-38",
            CliPassword,
            s(r#"\b(?:mysql|mysqldump|mysqladmin|mariadb)\b[^|;&\n]*?\s-p([^\s'"]{3,})"#),
            1,
            &[NP],
        ),
        (
            "SR-39",
            CliPassword,
            s(
                r#"\b(?:sshpass|docker\s+login|podman\s+login|helm\s+registry\s+login|az\s+login)\b[^|;&\n]*?\s-p\s+['"]?([^\s'"]{3,})"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-40",
            CliPassword,
            s(
                r#"\b(?:redis-cli\b[^|;&\n]*?\s-a|zip\b[^|;&\n]*?\s-P|openssl\b[^|;&\n]*?\s-k|security\s+add-(?:generic|internet)-password\b[^|;&\n]*?\s-w)\s+['"]?([^\s'"]{3,})"#,
            ),
            1,
            &[NP],
        ),
        (
            "SR-41",
            CliPassword,
            s(r#"\b(?:curl|wget)\b[^|;&\n]*?\s(?:-u|--user)[\s=]+['"]?[^\s:'"]+:([^\s'"@]{3,})"#),
            1,
            &[NP],
        ),
        (
            "SR-42",
            CliPassword,
            s(r#"\bhtpasswd\s+-b\S*\s+\S+\s+\S+\s+['"]?([^\s'"]{3,})"#),
            1,
            &[],
        ),
        (
            "SR-43",
            CliPassword,
            s(r#"\b(?:echo|printf)\s+['"]?([^\s'"|]{3,})['"]?\s*\|\s*sudo\s+-S"#),
            1,
            &[NP],
        ),
        (
            "SR-44",
            CliPassword,
            s(r#"\bsudo\s+-S\s*<<<\s*['"]?([^\s'"]{3,})"#),
            1,
            &[NP],
        ),
        (
            "SR-45",
            CliPassword,
            s(r"\baws\s+configure\s+set\s+\S*(?:secret|token)\S*\s+(\S{8,})"),
            1,
            &[NP],
        ),
        (
            "SR-46",
            NetrcPassword,
            s(r"\bmachine\s+\S+\s+login\s+\S+\s+password\s+(\S+)"),
            1,
            &[],
        ),
        (
            "SR-47",
            WifiPassword,
            s(r"\bWIFI:[^\s]*?\bP:([^;\s]+);"),
            1,
            &[],
        ),
    ]
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SelfCheckError {
    /// The named rule's expression failed to compile under G1–G3.
    Compile(&'static str),
    /// The combined set or a predicate expression failed to compile.
    Set,
    /// The canonical table no longer matches the recorded revision digest.
    Digest,
}
impl std::fmt::Display for SelfCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(id) => write!(f, "secret rule {id} failed to compile"),
            Self::Set => f.write_str("secret rule set failed to compile"),
            Self::Digest => f.write_str("secret rule set digest mismatch"),
        }
    }
}
impl std::error::Error for SelfCheckError {}

/// The compiled, immutable rule set. One per service; see `super::detection`.
pub struct RuleSet {
    pub(crate) rules: Vec<Rule>,
    expressions: Vec<Regex>,
    /// Rules per expression, in table order.
    expression_rules: Vec<Vec<usize>>,
    set: RegexSet,
    dotted: Regex,
    path: Regex,
    measure: Regex,
    digest: [u8; 32],
}
impl std::fmt::Debug for RuleSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuleSet")
            .field("revision", &RULE_SET_REVISION)
            .field("rules", &self.rules.len())
            .field("expressions", &self.expressions.len())
            .finish()
    }
}

fn build(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .unicode(false)
        .size_limit(EXPRESSION_SIZE_LIMIT)
        .dfa_size_limit(DFA_CACHE_LIMIT)
        .build()
}

impl RuleSet {
    /// Compiles every rule, the set and the predicate expressions, and checks
    /// the table digest against the recorded revision digest.
    pub fn compile() -> Result<Self, SelfCheckError> {
        let set = Self::compile_unchecked()?;
        if set.digest_hex() != RULE_SET_DIGEST {
            return Err(SelfCheckError::Digest);
        }
        Ok(set)
    }
    pub(crate) fn compile_unchecked() -> Result<Self, SelfCheckError> {
        let mut rules = Vec::new();
        let mut patterns: Vec<String> = Vec::new();
        let mut expressions = Vec::new();
        let mut expression_rules: Vec<Vec<usize>> = Vec::new();
        for (index, (id, kind, pattern, group, predicates)) in table().into_iter().enumerate() {
            let pattern = match pattern {
                Some(pattern) => {
                    expressions.push(build(&pattern).map_err(|_| SelfCheckError::Compile(id))?);
                    patterns.push(pattern.clone());
                    expression_rules.push(Vec::new());
                    pattern
                }
                None => patterns
                    .last()
                    .cloned()
                    .ok_or(SelfCheckError::Compile(id))?,
            };
            let expression = expressions.len() - 1;
            expression_rules[expression].push(index);
            rules.push(Rule {
                id,
                kind,
                pattern,
                group,
                predicates,
                expression,
            });
        }
        let set = RegexSetBuilder::new(&patterns)
            .unicode(false)
            .size_limit(EXPRESSION_SIZE_LIMIT * patterns.len())
            .dfa_size_limit(DFA_CACHE_LIMIT * 8)
            .build()
            .map_err(|_| SelfCheckError::Set)?;
        let helper = |p: &str| build(p).map_err(|_| SelfCheckError::Set);
        let digest = Sha256::digest(canonical_table(&rules)).into();
        Ok(Self {
            dotted: helper(r"^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)+$")?,
            path: helper(r"^[\w.\-]+(?:/[\w.\-]+)+\.[a-z]{1,5}$")?,
            measure: helper(r"^[0-9][0-9.,]*(?:[A-Za-z]{1,3}|%)?$")?,
            rules,
            expressions,
            expression_rules,
            set,
            digest,
        })
    }
    pub fn revision(&self) -> u32 {
        RULE_SET_REVISION
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn digest_hex(&self) -> String {
        self.digest.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
    pub fn expression_count(&self) -> usize {
        self.expressions.len()
    }

    /// Rule candidates in one line (or line fragment). In-line BEGIN…END pairs
    /// become `private-key` candidates. Returns the first BEGIN without a
    /// matching END in the fragment, which the stream layer turns into an open
    /// block (B1).
    pub(crate) fn candidates(&self, line: &[u8], out: &mut Vec<Candidate>) -> Option<Begin> {
        let mut begin = None;
        for expression in self.set.matches(line).iter() {
            if expression == 0 {
                begin = self.pem_pairs(line, out);
                continue;
            }
            for &rule in &self.expression_rules[expression] {
                self.rule_matches(rule, line, out);
            }
        }
        begin
    }

    fn pem_pairs(&self, line: &[u8], out: &mut Vec<Candidate>) -> Option<Begin> {
        let mut pos = 0;
        while let Some(caps) = self.expressions[0].captures_at(line, pos) {
            let whole = caps.get(0)?;
            let marker = end_marker(caps.get(1)?.as_bytes(), caps.get(2)?.as_bytes());
            let finder = EndFinder::new(marker);
            match finder.find(&line[whole.end()..]) {
                Some(end) => {
                    out.push(Candidate::rule(whole.start(), whole.end() + end, 0, 0));
                    pos = whole.end() + end;
                }
                None => {
                    return Some(Begin {
                        start: whole.start(),
                        finder,
                    });
                }
            }
        }
        None
    }

    fn rule_matches(&self, index: usize, line: &[u8], out: &mut Vec<Candidate>) {
        let rule = &self.rules[index];
        let regex = &self.expressions[rule.expression];
        let named = rule.has(Predicate::NameBoundary);
        let mut pos = 0;
        while pos <= line.len() {
            // The fast search finds the match bounds; structural predicates
            // need only those, so captures run only for a match they accept.
            let Some(whole) = regex.find_at(line, pos) else {
                break;
            };
            if named {
                // The prefix consumed the byte before the name unless the
                // name starts the line: names always start with [A-Za-z0-9].
                // Every named expression ends with its value group (SR-34–36)
                // or the value's closing quote (SR-33, A1).
                let name =
                    whole.start() + usize::from(!line[whole.start()].is_ascii_alphanumeric());
                let value_end = whole.end() - usize::from(rule.has(Predicate::NotContinued));
                let structural_ok = rule
                    .predicates
                    .iter()
                    .filter(|p| p.structural())
                    .all(|p| structural(*p, line, name, value_end));
                if !structural_ok {
                    // Restart so that the next name starts at name + 1 or later;
                    // the prefix byte may be the byte at `name`.
                    pos = name.max(1);
                    continue;
                }
            }
            let value = if rule.group == 0 {
                Some(whole.range())
            } else if named {
                let name =
                    whole.start() + usize::from(!line[whole.start()].is_ascii_alphanumeric());
                let value = named_value(line, name, whole.end(), rule.has(Predicate::NotContinued));
                debug_assert_eq!(
                    Some(value.clone()),
                    regex
                        .captures_at(line, whole.start())
                        .and_then(|caps| caps.get(rule.group))
                        .map(|m| m.range()),
                    "{}",
                    rule.id
                );
                Some(value)
            } else {
                regex
                    .captures_at(line, whole.start())
                    .and_then(|caps| caps.get(rule.group))
                    .map(|m| m.range())
            };
            if let Some(value) = value.filter(|v| !v.is_empty())
                && self.accepts(rule, &line[value.clone()])
            {
                out.push(Candidate::rule(
                    value.start,
                    value.end,
                    rule_order(index),
                    index,
                ));
            }
            // A named rule's next match may use the last byte as its prefix.
            pos = if named { whole.end() - 1 } else { whole.end() };
            if pos <= whole.start() {
                pos = whole.start() + 1;
            }
        }
    }

    fn accepts(&self, rule: &Rule, value: &[u8]) -> bool {
        rule.predicates.iter().all(|p| match p {
            Predicate::NotPlaceholder => !is_placeholder(value),
            Predicate::NotCode => !self.is_code(value),
            Predicate::ReadsAsCredential => self.reads_as_credential(value),
            _ => true,
        })
    }

    /// Wisp `isCode`: a type or keyword word, a leading operator, a dotted
    /// reference or a file path.
    pub(crate) fn is_code(&self, value: &[u8]) -> bool {
        const WORDS: &[&[u8]] = &[
            b"string",
            b"str",
            b"bool",
            b"boolean",
            b"int",
            b"integer",
            b"number",
            b"any",
            b"none",
            b"null",
            b"nil",
            b"true",
            b"false",
            b"undefined",
            b"required",
            b"optional",
            b"bytes",
            b"text",
            b"varchar",
            b"secretstr",
            b"data",
            b"self",
            b"this",
            b"value",
            b"input",
            b"env",
            b"config",
            b"params",
            b"args",
            b"options",
            b"await",
            b"try",
            b"new",
            b"let",
            b"var",
            b"const",
            b"function",
            b"lambda",
            b"return",
            b"async",
            b"yield",
        ];
        let lower = value.to_ascii_lowercase();
        let trimmed = trim(&lower, b"?!");
        if WORDS.contains(&trimmed) {
            return true;
        }
        if value.first().is_some_and(|b| b"+?-=/~.#".contains(b)) {
            return true;
        }
        self.dotted.is_match(value) || self.path.is_match(value)
    }

    /// Wisp `readsAsCredential`, with ASCII letters and digits (G1).
    pub(crate) fn reads_as_credential(&self, value: &[u8]) -> bool {
        value.iter().any(u8::is_ascii_alphabetic)
            && value.iter().any(u8::is_ascii_digit)
            && !self.measure.is_match(value)
    }
}

fn rule_order(index: usize) -> usize {
    index + 1
}

fn structural(p: Predicate, line: &[u8], name: usize, value_end: usize) -> bool {
    match p {
        Predicate::NotContinued => {
            // A1: value_end is the closing quote.
            let mut at = value_end + 1;
            while at < line.len() && is_ascii_space(line[at]) {
                at += 1;
            }
            !matches!(line.get(at), Some(b'+' | b'['))
        }
        Predicate::NotCallOrIndex => !matches!(line.get(value_end), Some(b'(' | b'[')),
        Predicate::KeyPosition => !sentence_position(line, name),
        Predicate::SentencePosition => sentence_position(line, name),
        _ => true,
    }
}

/// The value group of a credential-name rule (SR-33–SR-36) without a capture
/// search. A name is the maximal run of `[A-Za-z0-9_.-]` at `name`, since the
/// expression requires a quote, blank, `:` or `=` after it; then come an
/// optional quote, blanks, the separator, blanks and, for SR-33, the opening
/// quote. The value ends at the match end, or before the closing quote.
fn named_value(line: &[u8], name: usize, end: usize, quoted: bool) -> std::ops::Range<usize> {
    let mut at = name;
    while at < end && (line[at].is_ascii_alphanumeric() || matches!(line[at], b'_' | b'.' | b'-')) {
        at += 1;
    }
    at += usize::from(matches!(line.get(at), Some(b'"' | b'\'')));
    while at < end && is_ascii_space(line[at]) {
        at += 1;
    }
    at += 1;
    while at < end && is_ascii_space(line[at]) {
        at += 1;
    }
    at += usize::from(quoted);
    at.min(end)..end - usize::from(quoted)
}

/// L2: count the run of space and tab bytes before the name, examining at
/// most 17; a run of 1–16 after an ASCII letter or digit is sentence position.
pub(crate) fn sentence_position(line: &[u8], name: usize) -> bool {
    let mut k = 0;
    while k < 17 && name > k && matches!(line[name - k - 1], b' ' | b'\t') {
        k += 1;
    }
    (1..=16).contains(&k) && name > k && line[name - k - 1].is_ascii_alphanumeric()
}

/// ASCII whitespace as `regex` `\s` reads it without Unicode (G1).
pub(crate) fn is_ascii_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn trim<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let start = bytes
        .iter()
        .position(|b| !set.contains(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !set.contains(b))
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Wisp `isPlaceholder`, over bytes after ASCII lowercasing.
pub(crate) fn is_placeholder(value: &[u8]) -> bool {
    const WORDS: &[&[u8]] = &[
        b"changeme",
        b"password",
        b"secret",
        b"example",
        b"placeholder",
        b"your",
        b"dummy",
        b"fake",
        b"redacted",
        b"mock",
        b"sample",
        b"not-a-real",
        b"test-",
        b"...",
        "\u{2026}".as_bytes(),
        b"***",
        b"xxxx",
    ];
    const STOCK: &[&[u8]] = &[
        b"test",
        b"testing",
        b"postgres",
        b"root",
        b"guest",
        b"user",
        b"demo",
        b"none",
        b"null",
        b"todo",
        b"tbd",
    ];
    let lower = value.to_ascii_lowercase();
    let mut seen = [false; 256];
    let mut distinct = 0;
    for &b in &lower {
        if !seen[b as usize] {
            seen[b as usize] = true;
            distinct += 1;
        }
    }
    if distinct <= 2 {
        return true;
    }
    if WORDS.iter().any(|w| contains(&lower, w)) || STOCK.contains(&lower.as_slice()) {
        return true;
    }
    value.starts_with(b"$")
        || value.starts_with(b"<")
        || value.starts_with(b"{{")
        || value.starts_with(b"%")
}

pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

pub(crate) fn end_marker(label: &[u8], suffix: &[u8]) -> Vec<u8> {
    [
        b"-----END ".as_slice(),
        label,
        b"PRIVATE KEY",
        suffix,
        b"-----",
    ]
    .concat()
}

/// Linear search for one BEGIN block's END line marker.
pub(crate) struct EndFinder(aho_corasick::AhoCorasick);
impl EndFinder {
    fn new(marker: Vec<u8>) -> Self {
        // A single non-empty pattern within the line size never exceeds the
        // automaton's limits; fall back to a marker that cannot match.
        Self(
            aho_corasick::AhoCorasick::new([marker])
                .or_else(|_| aho_corasick::AhoCorasick::new(["-----END \0"]))
                .expect("fixed fallback marker builds"),
        )
    }
    /// The end offset of the first marker in `bytes`.
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<usize> {
        self.0.find(bytes).map(|m| m.end())
    }
}

pub(crate) struct Begin {
    pub start: usize,
    pub finder: EndFinder,
}

/// One candidate span in a line, before overlap merging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Candidate {
    pub start: usize,
    pub end: usize,
    pub kind: SecretKind,
    /// 0 for literals; table index + 1 for rules.
    pub order: usize,
    pub literal: Option<u32>,
}
impl Candidate {
    fn rule(start: usize, end: usize, order: usize, index: usize) -> Self {
        Self {
            start,
            end,
            kind: TABLE_KINDS[index],
            order: order.max(1),
            literal: None,
        }
    }
    /// An open private-key block that a long line cuts (B1 inside SD5).
    pub(crate) fn block(start: usize, end: usize) -> Self {
        Self::rule(start, end, 1, 0)
    }
    pub(crate) fn literal(start: usize, end: usize, kind: SecretKind, number: u32) -> Self {
        Self {
            start,
            end,
            kind,
            order: 0,
            literal: Some(number),
        }
    }
}

/// Kinds by table index, for building candidates without the table.
const TABLE_KINDS: [SecretKind; 47] = {
    use SecretKind::*;
    [
        PrivateKey,
        PrivateKey,
        AwsAccessKey,
        AwsSecretKey,
        GithubToken,
        GitlabToken,
        PypiToken,
        DockerToken,
        NpmToken,
        BitbucketToken,
        VaultToken,
        FlyToken,
        WebhookSecret,
        MailgunKey,
        GoogleOauth,
        AzureKey,
        SlackToken,
        SlackWebhook,
        ChatWebhook,
        StripeKey,
        AnthropicKey,
        OpenaiKey,
        GoogleApiKey,
        Jwt,
        UrlPassword,
        UrlSecret,
        AuthHeader,
        AuthHeader,
        SessionCookie,
        PasswordHash,
        AssignedSecret,
        EnvSecret,
        AssignedSecret,
        AssignedSecret,
        AssignedSecret,
        AssignedSecret,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        CliPassword,
        NetrcPassword,
        WifiPassword,
    ]
};

/// A merged span: the union of overlapping candidates (SD design step 2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Merged {
    pub start: usize,
    pub end: usize,
    pub kind: SecretKind,
    pub from_rule: bool,
    literal: Option<(usize, usize, u32)>,
}
impl Merged {
    /// The literal's number when the span is exactly one literal match.
    pub(crate) fn literal_number(&self) -> Option<u32> {
        self.literal
            .filter(|&(s, e, _)| s == self.start && e == self.end)
            .map(|(_, _, n)| n)
    }
}

/// Sort by start, longer first, then table order; merge each overlapping
/// candidate into the kept span. No byte of any candidate survives.
pub(crate) fn merge(mut candidates: Vec<Candidate>) -> Vec<Merged> {
    candidates.sort_by_key(|c| (c.start, std::cmp::Reverse(c.end), c.order));
    let mut merged: Vec<Merged> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let literal = c.literal.map(|n| (c.start, c.end, n));
        match merged.last_mut() {
            Some(last) if c.start < last.end => {
                last.end = last.end.max(c.end);
                last.from_rule |= c.literal.is_none();
                if last.literal.is_none() {
                    last.literal = literal;
                }
            }
            _ => merged.push(Merged {
                start: c.start,
                end: c.end,
                kind: c.kind,
                from_rule: c.literal.is_none(),
                literal,
            }),
        }
    }
    merged
}

/// Length-prefixed canonical table: for each rule, its ID, kind, pattern text,
/// value group and predicate identifiers, in order. Hashed with SHA-256.
fn canonical_table(rules: &[Rule]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |bytes: &[u8]| {
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
    };
    field(b"asura-secret-rules");
    field(&RULE_SET_REVISION.to_be_bytes());
    for rule in rules {
        field(rule.id.as_bytes());
        field(rule.kind.as_str().as_bytes());
        field(rule.pattern.as_bytes());
        field(&(rule.group as u32).to_be_bytes());
        let predicates: Vec<&str> = rule.predicates.iter().map(|p| p.id()).collect();
        field(predicates.join(",").as_bytes());
    }
    out
}
