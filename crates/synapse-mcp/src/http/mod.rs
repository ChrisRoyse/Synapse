mod auth;
mod session;
pub mod sse;
mod transport;

use std::{fmt, net::SocketAddr, process::ExitCode};

pub(crate) use auth::load_token_value;
pub(crate) use session::current_mcp_session_id;
pub(crate) use transport::http_transport_diagnostics_detail;

use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

#[derive(Debug)]
pub(crate) enum BindPreflightError {
    Invalid { bind: String, detail: String },
    NonLoopback { bind: SocketAddr },
}

impl fmt::Display for BindPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { bind, detail } => write!(
                formatter,
                "code={} bind={bind:?} detail={detail:?} expected_format=\"IP:PORT\" example=\"127.0.0.1:7700\" remediation=\"provide an IP socket address such as 127.0.0.1:7700\"",
                synapse_core::error_codes::HTTP_BIND_ADDRESS_INVALID,
            ),
            Self::NonLoopback { bind } => write!(
                formatter,
                "code={} bind={bind} remediation=\"bind to a loopback address, or explicitly pass --allow-non-loopback after securing the network boundary\"",
                synapse_core::error_codes::HTTP_BIND_NON_LOOPBACK_REFUSED,
            ),
        }
    }
}

/// Convert and authorize the HTTP endpoint before daemon-global startup.
///
/// This belongs above telemetry, QoS, input recovery, watchdog, storage, and
/// lifecycle initialization in `main::run`. Keeping the raw CLI field
/// mode-shared lets non-HTTP modes ignore it, while returning a typed address
/// here makes a second parse -- and therefore a second validation policy --
/// impossible.
pub(crate) fn preflight_bind(
    bind: &str,
    allow_non_loopback: bool,
) -> Result<SocketAddr, BindPreflightError> {
    let addr = match bind.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(error) => {
            return Err(BindPreflightError::Invalid {
                bind: bind.to_owned(),
                detail: error.to_string(),
            });
        }
    };
    if !addr.ip().is_loopback() && !allow_non_loopback {
        return Err(BindPreflightError::NonLoopback { bind: addr });
    }
    Ok(addr)
}

pub async fn serve(
    bind_addr: SocketAddr,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
    parent_watchdog: Option<tokio::sync::oneshot::Receiver<crate::connect::ParentWatchdogEvent>>,
) -> anyhow::Result<ExitCode> {
    transport::serve(bind_addr, m2_config, m3_config, m4_config, parent_watchdog).await
}
