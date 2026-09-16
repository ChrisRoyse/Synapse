//! Supporting regression check for cross-process UWP accessibility traversal.
//! This is not Full State Verification and does not replace manual FSV.
//! It launches the real Windows Calculator (a `Windows.UI.Core.CoreWindow`-
//! hosted UWP app whose XAML content runs in a separate process) and asserts
//! that the production `synapse_a11y::snapshot_window_from_hwnd` reaches the
//! display element across the cross-process boundary.
//!
//! Ignored by default because it requires an interactive Windows desktop
//! session. Run manually on a real host:
//!
//! ```text
//! cargo test -p synapse-a11y --test uwp_snapshot_regression -- --ignored --nocapture
//! ```
//!
//! Before this fix, `snapshot` collapsed to depth 1 (4 nodes, no content)
//! whenever the cross-process walk exceeded a 25ms latency guard. The
//! regression this guards against: the display element disappearing from the
//! tree.

#![cfg(windows)]

use std::error::Error;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Resolve a top-level window HWND by its UIA name through the production
/// MTA worker.
fn hwnd_for_window(name: &str) -> Result<Option<i64>, Box<dyn Error>> {
    Ok(synapse_a11y::top_level_window_hwnd_by_name(name)?)
}

#[test]
#[ignore = "requires an interactive Windows desktop session; run with --ignored"]
fn calculator_uwp_display_is_reachable_via_snapshot() -> Result<(), Box<dyn Error>> {
    // Launch Calculator if it is not already present.
    if hwnd_for_window("Calculator")?.is_none() {
        Command::new("cmd")
            .args(["/C", "start", "", "calc.exe"])
            .spawn()?;
    }

    // Wait for the window to register in the UIA tree (bounded).
    let deadline = Instant::now() + Duration::from_secs(15);
    let hwnd = loop {
        if let Some(hwnd) = hwnd_for_window("Calculator")? {
            break hwnd;
        }
        if Instant::now() >= deadline {
            return Err("Calculator window did not appear in the UIA tree".into());
        }
        sleep(Duration::from_millis(250));
    };

    let tree = synapse_a11y::snapshot_window_from_hwnd(hwnd, 12)?;

    // Regression oracle: the CoreWindow-hosted XAML display element, which
    // lives in a different process than the ApplicationFrameHost window.
    let display = tree.nodes.iter().find(|node| {
        node.automation_id.as_deref() == Some("CalculatorResults")
            || node.name.starts_with("Display is")
    });

    assert!(
        tree.nodes.len() > 10,
        "cross-process UWP content missing: snapshot only produced {} nodes (truncated={}); \
         the display element is hosted in a separate process and must be reached",
        tree.nodes.len(),
        tree.truncated,
    );
    assert!(
        display.is_some(),
        "Calculator display (source of truth) not found in {} snapshot nodes; \
         cross-process CoreWindow traversal regressed",
        tree.nodes.len(),
    );

    Ok(())
}
