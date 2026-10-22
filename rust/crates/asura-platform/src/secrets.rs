//! Secret detection and redaction (`docs/designs/secret-detection.md`, SL1).
//!
//! Pure computation over bytes: the rule set, predicates, overlap merge, the
//! streaming scanner with its 64 KiB carry, literal matching, per-job
//! numbering, loss markers and the command rewrite planner. The only system
//! call is the per-job random key. Detection is best effort (SR8).
mod arguments;
pub mod json;
pub mod rewrite;
mod rules;
mod stream;

pub use arguments::{
    ArgumentForm, ArgumentsInput, ExecCmdCommand, MAX_ARGUMENT_BYTES, MAX_ARGUMENT_NUMBERS,
    MAX_NAME_BYTES, MAX_STORED_NAME_BYTES, NotRetained, PlaceholderTokens, RedactedArguments,
    WITHHELD_TEXT, placeholder_tokens, redact_arguments,
};
pub use rewrite::{
    MAX_REWRITTEN_COMMAND_BYTES, MAX_WHOLE_COMMAND_NUMBERS, Plan, Refusal, RefusalCode, Rewrite,
    SECRET_COMMAND_NAME, SecretCommand, SecretVariable, WHOLE_COMMAND, WHOLE_COMMAND_NOTICE,
    WholeCommand, display_redacted, note_redacted, rewrite_command, rewrite_exec_cmd_call,
    rewrite_exec_cmd_call_with,
};
pub use rules::{RULE_SET_DIGEST, RULE_SET_REVISION, RuleSet, SelfCheckError};
pub use stream::{
    CARRY_LIMIT, JobRedaction, JobSummary, LossReason, LossSpan, MAX_LOSS_SPANS, MAX_NUMBERS,
    MAX_SUMMARY_NUMBERS, NumberInfo, NumberOrigin, PROPAGATED_BYTES_LIMIT, PROPAGATED_VALUES_LIMIT,
    Redacted, Stream, StreamStats,
};

use std::sync::OnceLock;

/// What a detected value is. Recorded in metadata, never in the placeholder (SD3).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum SecretKind {
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
    SessionCookie,
    PasswordHash,
    AssignedSecret,
    EnvSecret,
    CliPassword,
    NetrcPassword,
    WifiPassword,
}
impl SecretKind {
    pub const ALL: [SecretKind; 33] = {
        use SecretKind::*;
        [
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
            SessionCookie,
            PasswordHash,
            AssignedSecret,
            EnvSecret,
            CliPassword,
            NetrcPassword,
            WifiPassword,
        ]
    };
    /// Wisp's kind name.
    pub fn as_str(self) -> &'static str {
        use SecretKind::*;
        match self {
            PrivateKey => "private-key",
            AwsAccessKey => "aws-access-key",
            AwsSecretKey => "aws-secret-key",
            GithubToken => "github-token",
            GitlabToken => "gitlab-token",
            PypiToken => "pypi-token",
            DockerToken => "docker-token",
            NpmToken => "npm-token",
            BitbucketToken => "bitbucket-token",
            VaultToken => "vault-token",
            FlyToken => "fly-token",
            WebhookSecret => "webhook-secret",
            MailgunKey => "mailgun-key",
            GoogleOauth => "google-oauth",
            AzureKey => "azure-key",
            SlackToken => "slack-token",
            SlackWebhook => "slack-webhook",
            ChatWebhook => "chat-webhook",
            StripeKey => "stripe-key",
            AnthropicKey => "anthropic-key",
            OpenaiKey => "openai-key",
            GoogleApiKey => "google-api-key",
            Jwt => "jwt",
            UrlPassword => "url-password",
            UrlSecret => "url-secret",
            AuthHeader => "auth-header",
            SessionCookie => "session-cookie",
            PasswordHash => "password-hash",
            AssignedSecret => "assigned-secret",
            EnvSecret => "env-secret",
            CliPassword => "cli-password",
            NetrcPassword => "netrc-password",
            WifiPassword => "wifi-password",
        }
    }
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}
impl std::fmt::Display for SecretKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text that replaces value number `n`: `$ASURA_SECRET_<n>` (SD3).
pub fn placeholder(number: u32) -> String {
    format!("$ASURA_SECRET_{number}")
}

/// The compiled rule set and its self-check result, compiled once.
type RuleCell = OnceLock<Result<RuleSet, SelfCheckError>>;
/// The process's one rule set cell.
static RULES: RuleCell = OnceLock::new();

/// The process's compiled rule set, or the self-check failure that makes
/// every job start and command refuse with `secret_detection_unavailable`.
/// Compiles on first use; later calls return the same result. A call while
/// another thread compiles waits for that compile, so only worker threads
/// and tests call it (the warm thread, the capture worker), never the
/// reactor (secret-detection.md, rule set readiness, SD-S2).
pub fn detection() -> Result<&'static RuleSet, SelfCheckError> {
    detection_in(&RULES, RuleSet::compile)
}
fn detection_in(
    cell: &RuleCell,
    compile: impl FnOnce() -> Result<RuleSet, SelfCheckError>,
) -> Result<&RuleSet, SelfCheckError> {
    cell.get_or_init(compile).as_ref().map_err(Clone::clone)
}

/// The compiled rule set if it is ready, for reactor code. `None` means not
/// ready: no thread has finished compiling it. It reads `OnceLock::get`
/// only, so it never compiles and never blocks, also while another thread
/// compiles. `Some(Err)` is a failed self-check, and callers fail closed.
pub fn compiled() -> Option<Result<&'static RuleSet, SelfCheckError>> {
    compiled_in(&RULES)
}
fn compiled_in(cell: &RuleCell) -> Option<Result<&RuleSet, SelfCheckError>> {
    cell.get().map(|rules| rules.as_ref().map_err(Clone::clone))
}

#[cfg(test)]
mod tests;
