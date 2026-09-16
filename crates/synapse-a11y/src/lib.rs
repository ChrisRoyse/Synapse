#![allow(unsafe_code)]
#![allow(
    clippy::assigning_clones,
    clippy::branches_sharing_code,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::manual_let_else,
    clippy::map_unwrap_or,
    clippy::missing_errors_doc,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::needless_collect,
    clippy::option_if_let_else,
    clippy::significant_drop_tightening,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::too_long_first_doc_paragraph,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    reason = "synapse-a11y keeps pedantic/nursery style lint debt explicit while using clippy -D warnings for correctness and behavior regressions"
)]
#![cfg_attr(test, allow(clippy::float_cmp))]

mod cdp;
mod cdp_action;
mod cdp_actionability;
mod cdp_binding;
mod cdp_clock;
mod cdp_console;
mod cdp_dialog;
mod cdp_dom;
mod cdp_emulation;
mod cdp_files;
mod cdp_lifecycle;
mod cdp_network;
mod cdp_value;
mod error;
mod events;
mod ids;
mod platform;
mod re_resolve;
mod snapshot;
mod ui_element;
mod window;

pub use cdp::*;
#[cfg(windows)]
pub use cdp_action::*;
#[cfg(windows)]
pub use cdp_action::{CdpMouseStrokePoint, cdp_mouse_stroke_target};
#[cfg(windows)]
pub use cdp_actionability::*;
pub use cdp_binding::*;
pub use cdp_clock::*;
pub use cdp_console::*;
pub use cdp_dialog::*;
pub use cdp_dom::*;
pub use cdp_emulation::*;
pub use cdp_files::*;
pub use cdp_lifecycle::*;
pub use cdp_network::*;
pub use error::*;
pub use events::*;
pub use ids::*;
pub use re_resolve::*;
pub use snapshot::*;
pub use ui_element::*;
pub use window::millis_since_last_input;
pub use window::*;

#[cfg(test)]
mod tests;
