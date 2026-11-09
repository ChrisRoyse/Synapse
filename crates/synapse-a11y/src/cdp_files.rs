//! Typed CDP helpers for file uploads (#1101-#1103).
//!
//! Normal-profile file uploads route through the Chrome extension bridge, but
//! raw-CDP automation uses the same protocol primitives: `DOM.setFileInputFiles`
//! and `Page.setInterceptFileChooserDialog`/`Page.fileChooserOpened`.

use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, SetFileInputFilesParams};
use chromiumoxide::cdp::browser_protocol::page::{
    EventFileChooserOpened, SetInterceptFileChooserDialogParams,
};
use serde::Serialize;

use crate::{A11yError, A11yResult};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFileChooserEntry {
    pub seq: u64,
    pub frame_id: String,
    pub mode: String,
    pub backend_node_id: Option<i64>,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
}

pub fn cdp_set_file_input_files_params_for_backend_node(
    backend_node_id: i64,
    files: &[String],
) -> A11yResult<SetFileInputFilesParams> {
    if backend_node_id <= 0 {
        return Err(A11yError::CdpAttachFailed {
            detail: format!("backend_node_id must be positive, got {backend_node_id}"),
        });
    }
    SetFileInputFilesParams::builder()
        .files(files.iter().cloned())
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build()
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!("build DOM.setFileInputFiles params: {error}"),
        })
}

pub async fn cdp_set_file_input_files_by_backend_node(
    page: &Page,
    backend_node_id: i64,
    files: &[String],
) -> A11yResult<()> {
    let params = cdp_set_file_input_files_params_for_backend_node(backend_node_id, files)?;
    page.execute(params)
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("DOM.setFileInputFiles backendNodeId={backend_node_id}: {error}"),
        })?;
    Ok(())
}

pub fn cdp_intercept_file_chooser_params(
    enabled: bool,
    cancel: Option<bool>,
) -> A11yResult<SetInterceptFileChooserDialogParams> {
    let mut builder = SetInterceptFileChooserDialogParams::builder().enabled(enabled);
    if let Some(cancel) = cancel {
        builder = builder.cancel(cancel);
    }
    builder.build().map_err(|error| A11yError::CdpAttachFailed {
        detail: format!("build Page.setInterceptFileChooserDialog params: {error}"),
    })
}

pub async fn cdp_set_intercept_file_chooser(
    page: &Page,
    enabled: bool,
    cancel: Option<bool>,
) -> A11yResult<()> {
    let params = cdp_intercept_file_chooser_params(enabled, cancel)?;
    page.execute(params)
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("Page.setInterceptFileChooserDialog enabled={enabled}: {error}"),
        })?;
    Ok(())
}

#[must_use]
pub fn cdp_file_chooser_entry_from_event(
    event: &EventFileChooserOpened,
    seq: u64,
    opened_at_unix_ms: u64,
) -> CdpFileChooserEntry {
    CdpFileChooserEntry {
        seq,
        frame_id: event.frame_id.as_ref().to_owned(),
        mode: event.mode.as_ref().to_owned(),
        backend_node_id: event.backend_node_id.map(|id| *id.inner()),
        opened_at_unix_ms,
        pending: true,
    }
}
