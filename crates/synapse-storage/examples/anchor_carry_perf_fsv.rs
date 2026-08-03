//! Full-corpus performance FSV for #1981 against a writable vault backup.

use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use synapse_storage::{Db, cf};

const SCHEMA_VERSION: u32 = 1;
const PAGE_ROWS: usize = 1_000;

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: anchor_carry_perf_fsv <writable-backup-vault-directory>")?,
    );
    println!("SOURCE OF TRUTH: {}", vault_dir.display());
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let source_rows = db
        .cf_row_counts()?
        .get(cf::CF_AGENT_TRANSCRIPTS)
        .copied()
        .ok_or("CF_AGENT_TRANSCRIPTS count absent")?;
    let coverage_before = db.measure_panel_coverage()?;
    let transcript_before = coverage_before
        .panels
        .iter()
        .find(|panel| panel.panel_name == "syn-agent-transcript-v1")
        .ok_or("transcript panel absent")?;
    let base_before = coverage_before.base_cf_rows;
    let stranded_before = transcript_before.anchors_stranded_on_superseded;
    println!("BEFORE source_rows={source_rows} base_rows={base_before} stranded={stranded_before}");

    let started = Instant::now();
    let mut cursor: Option<Vec<u8>> = None;
    let mut pages = 0_u64;
    let mut candidates = 0_u64;
    let mut examined = 0_u64;
    let mut carried = 0_u64;
    let mut generations = 0_u64;
    let mut inserted = 0_u64;
    let mut already_current = 0_u64;
    loop {
        let page_started = Instant::now();
        let page = db.backfill_temporal_metadata(
            cf::CF_AGENT_TRANSCRIPTS,
            None,
            cursor.as_deref(),
            PAGE_ROWS,
        )?;
        pages += 1;
        candidates += page.candidate_rows_examined;
        examined += page.examined_rows;
        carried += page.anchors_carried_forward;
        generations += page.anchor_carry_source_generations_read;
        inserted += page.inserted_rows;
        already_current += page.already_current_rows;
        println!(
            "PAGE {pages:>3} elapsed_ms={:>7} candidates={} examined={} inserted={} current={} carried={} generation_reads={} more={}",
            page_started.elapsed().as_millis(),
            page.candidate_rows_examined,
            page.examined_rows,
            page.inserted_rows,
            page.already_current_rows,
            page.anchors_carried_forward,
            page.anchor_carry_source_generations_read,
            page.more,
        );
        if !page.more {
            break;
        }
        cursor = page.resume_after_physical;
        if cursor.is_none() {
            return Err("more=true without a resume cursor".into());
        }
    }

    let elapsed = started.elapsed();
    let coverage_after = db.measure_panel_coverage()?;
    let transcript_after = coverage_after
        .panels
        .iter()
        .find(|panel| panel.panel_name == "syn-agent-transcript-v1")
        .ok_or("transcript panel absent after sweep")?;
    println!(
        "AFTER pages={pages} candidates={candidates} examined={examined} inserted={inserted} current={already_current} carried={carried} generation_reads={generations} elapsed_ms={} base_rows={} stranded={}",
        elapsed.as_millis(),
        coverage_after.base_cf_rows,
        transcript_after.anchors_stranded_on_superseded
    );
    if examined != source_rows || candidates < examined {
        return Err(format!(
            "full sweep accounting failed: source_rows={source_rows} candidates={candidates} examined={examined}"
        )
        .into());
    }
    if coverage_after.base_cf_rows != base_before || inserted != 0 {
        return Err(format!(
            "already-current sweep changed Base population: before={base_before} after={} inserted={inserted}",
            coverage_after.base_cf_rows
        )
        .into());
    }
    if stranded_before > 0 && transcript_after.anchors_stranded_on_superseded != 0 {
        return Err(format!(
            "anchor debt did not converge: before={stranded_before} after={}",
            transcript_after.anchors_stranded_on_superseded
        )
        .into());
    }
    println!("PASS: full transcript population swept with physical row-guard readback");
    Ok(())
}
