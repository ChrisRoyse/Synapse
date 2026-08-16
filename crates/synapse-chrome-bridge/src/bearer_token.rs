//! Bearer-token resolution for the Synapse daemon and everything that
//! authenticates to it (#2099).
//!
//! This module is the **single** implementation of "where does the bearer token
//! come from". It lives in the shared Chrome bridge crate because both the
//! daemon and `synapse-chrome-native-host` authenticate through this boundary.
//! Before #2099 the native host carried its own copy of the resolution rule,
//! and a copy is exactly the thing that can silently disagree with the daemon
//! it is trying to talk to.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

pub const TOKEN_ENV: &str = "SYNAPSE_BEARER_TOKEN";
const APPDATA_ENV: &str = "APPDATA";

/// Which source supplied the token in force.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenSource {
    File(PathBuf),
    Env,
}

impl TokenSource {
    /// Stable, operator-facing name. Never contains the token value.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match *self {
            Self::File(_) => "file",
            Self::Env => "env",
        }
    }
}

/// The outcome of bearer-token resolution, with enough evidence to explain a
/// mismatch without ever disclosing a token value.
pub struct TokenResolution {
    pub token: String,
    pub source: TokenSource,
    /// The machine-shared token file path that was considered. Retained even
    /// when the env var won, because "the file exists and holds a *different*
    /// token" is exactly the condition that used to surface as an opaque 401.
    file_path: Option<PathBuf>,
    /// `Some(false)` when both sources supplied a token and they disagree;
    /// `Some(true)` when both supplied the same token; `None` when only one
    /// source supplied a token at all.
    sources_agree: Option<bool>,
}

/// Load the daemon bearer token value, for consumers that need only the value
/// (the `--mode connect` bridge, the local agent, the Chrome native host).
///
/// # Errors
///
/// Returns an error when neither source supplies a non-empty token, or when a
/// source that is present cannot be read.
pub fn load_token_value() -> anyhow::Result<String> {
    load_token().map(|resolution| resolution.token)
}

/// Resolve the daemon bearer token.
///
/// # Precedence: `SYNAPSE_BEARER_TOKEN` outranks `%APPDATA%\synapse\token.txt`
///
/// This is the conventional order, and the reason is that the two sources mean
/// different things:
///
/// * `%APPDATA%\synapse\token.txt` is the **machine-shared default**. It is
///   per-user — not per-process and not per-instance. Every daemon, every wired
///   MCP client and every script on the box reads that one path.
/// * `SYNAPSE_BEARER_TOKEN` is **per-process intent**. Something deliberately
///   placed that value in *this* process's environment, which is a strictly
///   more specific instruction than a file that happens to sit in a shared
///   directory.
///
/// Before #2099 the file won, so a daemon that had been handed its own token
/// could not use it. The generated supervisor exports the token file it was
/// configured with into `SYNAPSE_BEARER_TOKEN`, so an isolated instance bound
/// its own port, reported `"state":"running"`, and then rejected every request
/// made with the token it had been given — a mismatch that presented as an
/// opaque `/health` timeout with nothing saying "your token file was ignored".
///
/// Production behaviour is unchanged in both directions: the supervisor sets the
/// env var from the same `%APPDATA%` file the daemon would otherwise read, so
/// the two sources agree and the resolved token is identical; and with the env
/// var absent the file is still the resolution.
///
/// Resolution stays fail-closed. An empty or whitespace-only value from either
/// source is not a token and is an error rather than a reason to fall through to
/// the other source — falling through would let a broken explicit instruction be
/// masked by a different credential. When neither source supplies a token the
/// daemon refuses to start rather than serving unauthenticated.
///
/// # Errors
///
/// Returns an error naming both consulted sources when neither supplies a
/// non-empty token, or when a present source cannot be read.
pub fn load_token() -> anyhow::Result<TokenResolution> {
    let file_path = token_file_path();
    let file_token = read_token_file(file_path.as_deref())?;
    let env_token = read_env_token()?;
    match (env_token, file_token) {
        (Some(env_token), file_token) => {
            let sources_agree = file_token.map(|file_token| file_token == env_token);
            Ok(TokenResolution {
                token: env_token,
                source: TokenSource::Env,
                file_path,
                sources_agree,
            })
        }
        (None, Some(file_token)) => {
            let path = file_path.clone().ok_or_else(|| {
                anyhow::anyhow!("token file supplied a token without a resolved path")
            })?;
            Ok(TokenResolution {
                token: file_token,
                source: TokenSource::File(path),
                file_path,
                sources_agree: None,
            })
        }
        (None, None) => bail!(
            "no HTTP bearer token: {TOKEN_ENV} is unset, and the token file {} is absent. Set \
             {TOKEN_ENV} for a per-instance token, or create the machine-shared token file. \
             Serving without a token is refused.",
            file_path.map_or_else(
                || format!("(unresolvable: %{APPDATA_ENV}% is unset)"),
                |path| path.display().to_string()
            )
        ),
    }
}

impl TokenResolution {
    /// Name the winning source at boot. Never logs a token value — only which
    /// source won, the file path considered, and whether the two sources agree.
    ///
    /// A disagreement is logged at WARN because it is the exact state that used
    /// to be undiagnosable: every client holding the *other* token gets a bare
    /// `401 HTTP_TOKEN_INVALID`, and on a health probe that reads as a timeout
    /// rather than as an auth mismatch.
    pub fn report(&self) {
        let file_path = self.file_path.as_ref().map_or_else(
            || "<unresolved>".to_owned(),
            |path| path.display().to_string(),
        );
        if self.sources_agree == Some(false) {
            tracing::warn!(
                code = "MCP_HTTP_TOKEN_SOURCE_CONFLICT",
                token_source = self.source.label(),
                token_env_var = TOKEN_ENV,
                token_file_path = %file_path,
                "both {TOKEN_ENV} and the token file supplied a bearer token and they differ; the \
                 environment variable wins because it is per-process intent while the file is the \
                 machine-shared default. Any client authenticating with the token file will be \
                 rejected with HTTP_TOKEN_INVALID."
            );
            return;
        }
        tracing::info!(
            code = "MCP_HTTP_TOKEN_SOURCE_RESOLVED",
            token_source = self.source.label(),
            token_env_var = TOKEN_ENV,
            token_file_path = %file_path,
            token_file_present = self.file_path.as_ref().is_some_and(|path| path.is_file()),
            sources_agree = ?self.sources_agree,
            "resolved the HTTP bearer token source"
        );
    }
}

/// Read the machine-shared token file. A missing file is not an error; an
/// unreadable or empty one is, because silently falling through would hide a
/// real misconfiguration behind a different credential.
fn read_token_file(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read HTTP bearer token file {}", path.display()))?;
    normalize_token(&raw)
        .map(Some)
        .with_context(|| format!("HTTP bearer token file is empty: {}", path.display()))
}

/// Read `SYNAPSE_BEARER_TOKEN`. Unset is not an error; set-but-empty is, because
/// an empty explicit instruction is a mistake, not a request to fall back.
fn read_env_token() -> anyhow::Result<Option<String>> {
    let Ok(raw) = std::env::var(TOKEN_ENV) else {
        return Ok(None);
    };
    normalize_token(&raw)
        .map(Some)
        .with_context(|| format!("{TOKEN_ENV} is set but empty"))
}

fn token_file_path() -> Option<PathBuf> {
    let appdata = std::env::var_os(APPDATA_ENV)?;
    Some(PathBuf::from(appdata).join("synapse").join("token.txt"))
}

fn normalize_token(raw: &str) -> anyhow::Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("empty token")
    }
    Ok(token.to_owned())
}
