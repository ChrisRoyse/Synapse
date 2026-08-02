//! Full-state verification for issue #1958 ask 3, end to end on a real vault.
//!
//! `anchor_source_provenance_fsv` proves the declaration tables give the right
//! answer. This proves the answer is actually *wired into the assay* — that a
//! measurement adjudicated by an anchor over a panel containing a lens that
//! reads that anchor's determining fields fails closed, on the live corpus,
//! before any bits are spent; and that the remediation the refusal names
//! produces a real report.
//!
//! ## What was happening before
//!
//! On `syn-mcp-usage-v1 @ 1776006` with `synapse:mcp_tool_call_outcome`:
//!
//! ```text
//! pass 1  nothing withheld     panel_bits=0.969018  floor=true   detector: [86]
//! pass 2  withheld {86}        panel_bits=0.946621  floor=true   detector: CLEAN
//! pass 3  withheld {86,87,93}  panel_bits=0.958031  floor=false  detector: CLEAN
//! ```
//!
//! Pass 2 is the outcome this issue was filed over: a `CLEAN` verdict over a
//! number that is floored on slot 93, itself a carrier. Circular, and reported
//! as clean.
//!
//! ## What must happen now
//!
//! | pass | withheld | expected |
//! |---|---|---|
//! | 1 | nothing | **refused**, naming 86, 87 and 93 |
//! | 2 | {86} | **refused**, naming 87 and 93 — the case that used to read CLEAN |
//! | 3 | {86, 87} | **refused**, naming 93 — the dense carrier alone |
//! | 4 | {86, 87, 93} | a report, `anchor_source_declared = true` |
//!
//! Pass 4 is the load-bearing one in the other direction: a refusal that cannot
//! be lifted is a refusal that makes the panel unmeasurable, which is the
//! failure mode #1958 warns about when it says lowering the detector's threshold
//! "would make a good panel unmeasurable".
//!
//! Run against a **frozen copy** of the vault, never the live one: a live vault
//! is a moving corpus, so any before/after over it measures drift as well as the
//! change (`docs/BUILD-AND-MAINTENANCE.md`).
//!
//! ```text
//! cargo run --release -p synapse-calyx --example anchor_source_refusal_fsv -- <vault-copy-dir>
//! ```

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::lens_provenance::syn_anchor_source_provenance;
use synapse_calyx::{
    SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE, SynapseCalyxAssayParams, SynapseCalyxConfig,
    SynapseCalyxTuningConfig, SynapseCalyxVault,
};

const PANEL: u32 = 1_776_006;
const PANEL_NAME: &str = "syn-mcp-usage-v1";
const ANCHOR: &str = "synapse:mcp_tool_call_outcome";
const MAX_RECORDS: usize = 4_000;

struct Pass {
    withheld: &'static [u16],
    /// `None` = must produce a report; `Some(slots)` = must be refused naming
    /// exactly these.
    refused_naming: Option<&'static [u16]>,
    note: &'static str,
}

const PASSES: &[Pass] = &[
    Pass {
        withheld: &[],
        refused_naming: Some(&[86, 87, 93]),
        note: "the whole panel; all three carriers",
    },
    Pass {
        withheld: &[86],
        refused_naming: Some(&[87, 93]),
        note: "#1958's pass 2 — this is what used to report CLEAN over a floored, circular number",
    },
    Pass {
        withheld: &[86, 87],
        refused_naming: Some(&[93]),
        note: "the dense carrier alone; no statistical test can reach this one",
    },
    Pass {
        withheld: &[86, 87, 93],
        refused_naming: None,
        note: "the remediation the refusal names — must produce a real report",
    },
];

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: anchor_source_refusal_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxVault::open(config)?;

    println!("anchor_source_refusal_fsv  (#1958 ask 3)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq        = {}", vault.latest_seq());
    println!("panel             = {PANEL_NAME} @ {PANEL}");
    println!("anchor            = {ANCHOR}");

    let declared = syn_anchor_source_provenance(ANCHOR, PANEL, &BTreeSet::new());
    println!(
        "anchor determined by [{}]\n",
        declared.anchor_fields.join(", ")
    );
    if !declared.anchor_declared {
        return Err(
            "the pair under test is undeclared; every pass below would pass \
                    vacuously"
                .into(),
        );
    }

    for (index, pass) in PASSES.iter().enumerate() {
        let mut params = SynapseCalyxAssayParams::new(PANEL, ANCHOR.to_owned());
        params.max_records = MAX_RECORDS;
        params.excluded_slots = pass.withheld.iter().copied().collect::<BTreeSet<u16>>();
        println!(
            "=== pass {} withheld={:?}\n    {}",
            index + 1,
            pass.withheld,
            pass.note
        );
        let outcome = vault.assay_sufficiency(&params);
        match (&outcome, pass.refused_naming) {
            (Err(error), Some(expected)) => {
                println!("    REFUSED  code = {}", error.code);
                println!("    message  = {}", error.message);
                if error.code != SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE {
                    return Err(format!(
                        "pass {} refused with {} rather than the structural leakage code",
                        index + 1,
                        error.code
                    )
                    .into());
                }
                // The refusal has to NAME the carriers, or a caller cannot act
                // on it. Checked against the message text because that is what
                // actually crosses the boundary to a human or an agent.
                for slot in expected {
                    let needle = format!("slot {slot} ");
                    if !error.message.contains(&needle) {
                        return Err(format!(
                            "pass {} did not name slot {slot} in its message",
                            index + 1
                        )
                        .into());
                    }
                }
                for slot in pass.withheld {
                    if error.message.contains(&format!("slot {slot} (")) {
                        return Err(format!(
                            "pass {} named withheld slot {slot} as a carrier; excluding a \
                             slot must remove it from the panel under measurement",
                            index + 1
                        )
                        .into());
                    }
                }
                let remediation = format!(
                    "excluded_slots=[{}]",
                    expected
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                );
                if !error.message.contains(&remediation) {
                    return Err(format!(
                        "pass {} did not carry the remediation set {remediation}",
                        index + 1
                    )
                    .into());
                }
                println!("    names exactly {expected:?} and carries the remediation set");
            }
            (Ok(report), None) => {
                println!(
                    "    REPORT   anchored={} joint={} panel_bits={:.6} H(anchor)={:.6}",
                    report.anchored_records,
                    report.joint_records,
                    report.panel_bits,
                    report.anchor_entropy_bits
                );
                println!(
                    "    measured={} floor_applied={} sufficient={} deficit={:.6}",
                    report.panel_measured,
                    report.panel_floor_applied,
                    report.sufficient,
                    report.deficit_bits
                );
                println!(
                    "    anchor_source_declared={} statistical_leakage={}",
                    report.anchor_source_declared,
                    report.anchor_leakage.len()
                );
                if !report.anchor_source_declared {
                    return Err(
                        "the surviving report does not say the structural check ran; \
                                a clean report and an unchecked one must not look alike"
                            .into(),
                    );
                }
                if report.anchored_records == 0 {
                    return Err("the surviving pass measured zero anchored records, so it \
                                proves nothing about the corpus"
                        .into());
                }
                if !report.anchor_leakage.is_empty() {
                    return Err(format!(
                        "the statistical detector still flags {:?} after the structural \
                         carriers were withheld",
                        report.anchor_leakage
                    )
                    .into());
                }
            }
            (Ok(report), Some(expected)) => {
                return Err(format!(
                    "pass {} SUCCEEDED with panel_bits={:.6} floor_applied={} and should have \
                     been refused naming {expected:?} — this is the #1958 defect",
                    index + 1,
                    report.panel_bits,
                    report.panel_floor_applied
                )
                .into());
            }
            (Err(error), None) => {
                return Err(format!(
                    "pass {} was refused ({}) after the named carriers were withheld — a \
                     refusal that cannot be lifted makes the panel unmeasurable: {}",
                    index + 1,
                    error.code,
                    error.message
                )
                .into());
            }
        }
        println!();
    }

    println!("PASS: every circular configuration is refused by name, and the remediation works.");
    Ok(())
}
