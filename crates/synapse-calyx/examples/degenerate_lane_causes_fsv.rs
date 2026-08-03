//! Manual FSV for #1970: does the lens-coverage instrument name **every** way a
//! lane can fail to rank, or only the one someone thought of first?
//!
//! ## The claim under test
//!
//! #1970 was filed because `syn.agent_event.usage_total_log1p.v1` (slot 33 on
//! `syn-agent-event-v1`) sums four GenAI token fields that are absent on every
//! row of `CF_AGENT_EVENTS` — that CF is Synapse's agent *lifecycle* log, not
//! model-completion telemetry. The instrument added in response,
//! `CALYX_LENS_CONSTANT_BY_CORPUS`, shipped and went live — and **it does not
//! report slot 33.** It reports two other lanes.
//!
//! The reason is structural, not a tuning miss. `optional_log1p_slot` emits
//! `absent(AbsentReason::NotApplicable)` when its input is `None`, so slot 33 is
//! `Absent` on every record: it is never *measured*, never lands in
//! `record.slots`, and so can never reach the constant detector's
//! `records_present >= 8` floor. It fails the FIRST clause of that filter, not
//! the second. An instrument built for one failure mode could not see a
//! different one.
//!
//! There are three distinct ways a lane cannot rank, and this proves the
//! instrument now names all three over a real corpus:
//!
//! - `CALYX_LENS_CONSTANT_BY_CORPUS` — present everywhere, one value.
//! - `CALYX_LENS_ABSENT_BY_CORPUS` — never present at all.
//! - `CALYX_LENS_SINGLE_SUPPORT_BY_CORPUS` — a sparse lane densified to a
//!   one-index support.
//!
//! The third is arithmetic rather than statistical: the cosine between any two
//! same-sign one-dimensional vectors is `ab / (|a|*|b|) = 1` exactly, so such a
//! lane cannot order neighbours even though its *values* vary — which is
//! precisely why the constant check does not fire on it.
//!
//! ## Where the truth is read from
//!
//! Not from a fixture. From a frozen copy of the **live production vault**, and
//! the expectations below were derived from a *different* source of truth: a
//! `storage operation=intelligence {operation: abundance, panel_version:
//! 1665001}` call against the running daemon, which independently reported
//! `slot 26 kind=absent`, `slot 33 kind=absent`, `slot 25 densified_support=1`
//! and `slot 27 densified_support=2`. So this is a cross-check between two
//! independent paths over the same corpus, not a probe agreeing with itself.
//!
//! Slot 27 is the **boundary case and the control**: support 2, one above the
//! rule's threshold. An instrument that flagged it would be over-reporting, and
//! an instrument that cannot separate 1 from 2 is measuring nothing.
//!
//! ```text
//! cargo run -p synapse-calyx --example degenerate_lane_causes_fsv -- <vault-copy-dir>
//! ```
//!
//! `<vault-copy-dir>` must contain `db-daemon/` and `machine-salt.bin`. Use a
//! copy: this takes the writer lock, and the live vault already holds it.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault};

const CONSTANT: &str = "CALYX_LENS_CONSTANT_BY_CORPUS";
const ABSENT: &str = "CALYX_LENS_ABSENT_BY_CORPUS";
const SINGLE: &str = "CALYX_LENS_SINGLE_SUPPORT_BY_CORPUS";

/// Every panel generation the live vault carries, from
/// `storage operation=panel_coverage`.
const PANELS: &[u32] = &[
    1_963_001, 1_776_001, 1_776_002, 1_776_003, 1_776_004, 1_776_005, 1_776_006, 1_776_007,
    1_685_001, 1_685_002, 1_685_003, 1_964_001, 1_665_001, 1_921_001,
];

/// One falsifiable prediction about the live corpus.
struct Expect {
    panel: u32,
    slot: u16,
    lens: &'static str,
    /// `Some(code)` = must be reported with exactly this code.
    /// `None`       = must NOT be reported at all.
    code: Option<&'static str>,
    why: &'static str,
}

const EXPECTATIONS: &[Expect] = &[
    // The lane this issue is ABOUT, which the shipped instrument could not see.
    Expect {
        panel: 1_665_001,
        slot: 33,
        lens: "syn.agent_event.usage_total_log1p.v1",
        code: Some(ABSENT),
        why: "all four GenAI token fields are absent on every CF_AGENT_EVENTS row",
    },
    // #1970 ask 3 predicted this one reads the same GenAiAttributes block.
    Expect {
        panel: 1_665_001,
        slot: 26,
        lens: "syn.agent_event.request_model_hash.v1",
        code: Some(ABSENT),
        why: "same GenAiAttributes block as slot 33",
    },
    // PRECEDENCE. This lane is width-1 AND single-valued (the run reports
    // distinct=1 over the 231 records carrying it), so both the constant and
    // the single-support causes are true of it. It must be reported ONCE, as
    // CONSTANT — the stricter and more directly actionable statement. My first
    // pass predicted SINGLE here and the FSV refuted it; the corpus, not the
    // prediction, is the authority.
    Expect {
        panel: 1_665_001,
        slot: 25,
        lens: "syn.agent_event.provider_hash.v1",
        code: Some(CONSTANT),
        why: "width 1 AND one distinct value over 231 records; constant is the stricter cause",
    },
    // FLOOR, found by this FSV. Before the min-records floor was applied to the
    // single-support cause, these two were reported as dead lanes from corpora
    // of TWO and ONE record respectively -- where a support of 1 is
    // arithmetically unavoidable and says nothing about the lens. A degeneracy
    // claim needs a corpus to be a claim about.
    Expect {
        panel: 1_776_003,
        slot: 60,
        lens: "syn-process-v1 sparse lane",
        code: None,
        why: "only 2 records carry it, below the 8-record floor",
    },
    Expect {
        panel: 1_776_004,
        slot: 67,
        lens: "syn-observation-v1 sparse lane",
        code: None,
        why: "only 1 record carries it; support 1 is unavoidable at n=1",
    },
    // REGRESSION GUARD: the pre-existing detector must still fire.
    Expect {
        panel: 1_665_001,
        slot: 30,
        lens: "syn.agent_event.end_state_onehot.v1",
        code: Some(CONSTANT),
        why: "the lane health already reports as 1665001:30",
    },
    Expect {
        panel: 1_921_001,
        slot: 36,
        lens: "syn.agent_transcript.status_onehot.v1",
        code: Some(CONSTANT),
        why: "the lane health already reports as 1921001:36",
    },
    // BOUNDARY / CONTROL: support 2 is one above the rule. Must NOT be flagged.
    Expect {
        panel: 1_665_001,
        slot: 27,
        lens: "syn.agent_transcript.response_model_hash.v1",
        code: None,
        why: "densified_support = 2, one above the single-support threshold",
    },
    // CONTROL: a dense lane that genuinely varies must stay unflagged.
    Expect {
        panel: 1_665_001,
        slot: 23,
        lens: "syn.agent_event.kind_onehot.v1",
        code: None,
        why: "kind varies across tool_call_started/state_changed/exited/...",
    },
];

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: degenerate_lane_causes_fsv <vault-copy-dir>")?;
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

    println!("degenerate_lane_causes_fsv  (#1970)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq        = {}", vault.latest_seq());

    // 20,000 records per panel: above every panel's live population except the
    // transcript corpus, so "absent on every record" is a statement about the
    // whole panel rather than about a sample of it.
    let coverage = vault.lens_coverage_status(PANELS, 20_000)?;

    println!("\n== every lane the instrument reports as unable to rank ==");
    println!(
        "{:<10} {:<6} {:<38} {:>9} {:>9}",
        "panel", "slot", "code", "present", "distinct"
    );
    for lane in &coverage.degenerate_lanes {
        println!(
            "{:<10} {:<6} {:<38} {:>9} {:>9}",
            lane.panel_version, lane.slot, lane.code, lane.records_present, lane.distinct_values
        );
    }

    let by_cause = |code: &str| {
        coverage
            .degenerate_lanes
            .iter()
            .filter(|l| l.code == code)
            .count()
    };
    println!(
        "\ntotals: constant={} absent={} single_support={} (all={})",
        by_cause(CONSTANT),
        by_cause(ABSENT),
        by_cause(SINGLE),
        coverage.degenerate_lanes.len()
    );

    let mut failures: Vec<String> = Vec::new();

    println!("\n== falsifiable predictions, derived from an INDEPENDENT daemon readback ==");
    for expect in EXPECTATIONS {
        let found = coverage
            .degenerate_lanes
            .iter()
            .find(|l| l.panel_version == expect.panel && l.slot == expect.slot);
        let actual = found.map(|l| l.code.as_str());
        let ok = actual == expect.code;
        if !ok {
            failures.push(format!(
                "panel {} slot {} ({}): expected {:?}, got {:?}",
                expect.panel, expect.slot, expect.lens, expect.code, actual
            ));
        }
        println!(
            "  [{}] {}:{:<3} {:<44} expected={:<38} actual={}",
            if ok { "PASS" } else { "FAIL" },
            expect.panel,
            expect.slot,
            expect.lens,
            expect.code.unwrap_or("<not reported>"),
            actual.unwrap_or("<not reported>")
        );
        println!("         because: {}", expect.why);
    }

    println!("\n== structural invariants ==");

    // An absent lane was never measured, so its presence count MUST be zero.
    // A non-zero value would mean the loader marked a slot absent for the panel
    // while some record carried a real vector for it, which would make the
    // ABSENT verdict a lie.
    let bad_absent: Vec<String> = coverage
        .degenerate_lanes
        .iter()
        .filter(|l| l.code == ABSENT && l.records_present != 0)
        .map(|l| {
            format!(
                "{}:{} records_present={}",
                l.panel_version, l.slot, l.records_present
            )
        })
        .collect();
    report(
        &mut failures,
        "every ABSENT lane has records_present == 0",
        bad_absent.is_empty(),
        &bad_absent.join(", "),
    );

    // A constant lane is by definition one distinct value.
    let bad_constant: Vec<String> = coverage
        .degenerate_lanes
        .iter()
        .filter(|l| l.code == CONSTANT && l.distinct_values != 1)
        .map(|l| {
            format!(
                "{}:{} distinct={}",
                l.panel_version, l.slot, l.distinct_values
            )
        })
        .collect();
    report(
        &mut failures,
        "every CONSTANT lane has distinct_values == 1",
        bad_constant.is_empty(),
        &bad_constant.join(", "),
    );

    // A single-support lane densifies to width <= 1.
    let bad_single: Vec<String> = coverage
        .degenerate_lanes
        .iter()
        .filter(|l| l.code == SINGLE && l.distinct_values > 1)
        .map(|l| format!("{}:{} width={}", l.panel_version, l.slot, l.distinct_values))
        .collect();
    report(
        &mut failures,
        "every SINGLE_SUPPORT lane has width <= 1",
        bad_single.is_empty(),
        &bad_single.join(", "),
    );

    // No lane may be reported twice. The three detectors overlap by
    // construction (a width-1 lane can also be constant), so the dedup is
    // load-bearing: a double report would double-count the vault's dead lenses.
    let mut seen: BTreeSet<(u32, u16)> = BTreeSet::new();
    let mut dupes: Vec<String> = Vec::new();
    for lane in &coverage.degenerate_lanes {
        if !seen.insert((lane.panel_version, lane.slot)) {
            dupes.push(format!("{}:{}", lane.panel_version, lane.slot));
        }
    }
    report(
        &mut failures,
        "no (panel, slot) is reported more than once",
        dupes.is_empty(),
        &dupes.join(", "),
    );

    // The evidence floor. A lane that WAS measured must have been measured on
    // enough records for "it never varied" to mean anything. Absent lanes are
    // exempt by definition: they carry zero because they were never measured at
    // all, which is the finding rather than a thin sample.
    let thin: Vec<String> = coverage
        .degenerate_lanes
        .iter()
        .filter(|l| l.code != ABSENT && l.records_present < 8)
        .map(|l| {
            format!(
                "{}:{} present={}",
                l.panel_version, l.slot, l.records_present
            )
        })
        .collect();
    report(
        &mut failures,
        "no measured lane is called degenerate from fewer than 8 records",
        thin.is_empty(),
        &thin.join(", "),
    );

    // Fail-closed discipline: every finding must carry an actionable code,
    // detail and remediation. A finding an operator cannot act on is noise.
    let empty_text: Vec<String> = coverage
        .degenerate_lanes
        .iter()
        .filter(|l| l.code.is_empty() || l.detail.is_empty() || l.remediation.is_empty())
        .map(|l| format!("{}:{}", l.panel_version, l.slot))
        .collect();
    report(
        &mut failures,
        "every lane carries code + detail + remediation",
        empty_text.is_empty(),
        &empty_text.join(", "),
    );

    // The instrument must not have regressed into reporting everything: a panel
    // whose lanes are ALL degenerate would mean the detector is broken, not that
    // the panel is dead.
    let mut over_report: Vec<String> = Vec::new();
    for panel in &coverage.panels {
        let flagged = coverage
            .degenerate_lanes
            .iter()
            .filter(|l| l.panel_version == panel.panel_version)
            .count();
        if panel.n_lenses > 0 && flagged == panel.n_lenses {
            over_report.push(format!(
                "{} flagged {}/{}",
                panel.panel_version, flagged, panel.n_lenses
            ));
        }
    }
    report(
        &mut failures,
        "no panel has 100% of its lanes flagged",
        over_report.is_empty(),
        &over_report.join(", "),
    );

    println!("\n== per-panel context ==");
    for panel in &coverage.panels {
        let flagged = coverage
            .degenerate_lanes
            .iter()
            .filter(|l| l.panel_version == panel.panel_version)
            .count();
        println!(
            "  panel {:<10} lenses={:<3} measured={:<6} scanned={:<6} unable_to_rank={}",
            panel.panel_version,
            panel.n_lenses,
            panel.records_measured,
            panel.records_scanned,
            flagged
        );
    }

    println!();
    if failures.is_empty() {
        println!("degenerate_lane_causes_fsv: ALL CHECKS PASS");
        return Ok(());
    }
    println!("degenerate_lane_causes_fsv: {} FAILURE(S)", failures.len());
    for failure in &failures {
        println!("  - {failure}");
    }
    Err("FSV failed".into())
}

fn report(failures: &mut Vec<String>, label: &str, ok: bool, detail: &str) {
    println!(
        "  [{}] {label}{}",
        if ok { "PASS" } else { "FAIL" },
        if ok || detail.is_empty() {
            String::new()
        } else {
            format!("  -> {detail}")
        }
    );
    if !ok {
        failures.push(format!("{label}: {detail}"));
    }
}
