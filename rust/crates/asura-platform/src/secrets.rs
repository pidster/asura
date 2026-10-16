//! Secret detection and redaction (`docs/designs/secret-detection.md`, SL1).
//!
//! Pure computation over bytes: the rule set, predicates, overlap merge, the
//! streaming scanner with its 64 KiB carry, literal matching, per-job
//! numbering, loss markers and the command rewrite planner. The only system
//! call is the per-job random key. Detection is best effort (SR8).
pub mod rewrite;
mod rules;
mod stream;

pub use rewrite::{
    MAX_REWRITTEN_COMMAND_BYTES, Refusal, RefusalCode, Rewrite, SecretVariable, display_redacted,
    rewrite_command, rewrite_shell_call,
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

/// The process's compiled rule set, or the self-check failure that makes
/// every job start and command refuse with `secret_detection_unavailable`.
/// Compiled once; later calls return the same result.
pub fn detection() -> Result<&'static RuleSet, SelfCheckError> {
    static RULES: OnceLock<Result<RuleSet, SelfCheckError>> = OnceLock::new();
    RULES
        .get_or_init(RuleSet::compile)
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests;
