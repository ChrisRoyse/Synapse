mod auth;
mod session;
pub mod sse;
mod transport;

pub(crate) use auth::load_token_value;
pub(crate) use session::current_mcp_session_id;
#[cfg(test)]
pub(crate) use session::with_current_mcp_session_id_for_test;
pub(crate) use transport::http_transport_diagnostics_detail;

use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

pub async fn serve(
    bind: &str,
    allow_non_loopback: bool,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
) -> anyhow::Result<std::process::ExitCode> {
    transport::serve(bind, allow_non_loopback, m2_config, m3_config, m4_config).await
}
