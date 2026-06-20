#![allow(unsafe_code)]

mod cdp;
mod cdp_action;
mod cdp_console;
mod cdp_dom;
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
pub use cdp_console::*;
pub use cdp_dom::*;
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
