use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Result};
use serde::{Deserialize, Serialize};

pub const CALYX_PANEL_GENERATION_INVALID: &str = "CALYX_PANEL_GENERATION_INVALID";
pub const CALYX_PANEL_GENERATION_CONFLICT: &str = "CALYX_PANEL_GENERATION_CONFLICT";
pub const CALYX_PANEL_GENERATION_EXHAUSTED: &str = "CALYX_PANEL_GENERATION_EXHAUSTED";
/// A generation named by a writer that neither a reservation nor an allocation
/// ever claimed (#2062 ask 2).
pub const CALYX_PANEL_GENERATION_UNCLAIMED: &str = "CALYX_PANEL_GENERATION_UNCLAIMED";

/// Lowest generation a **dynamic** allocation may ever mint, and the ceiling no
/// **built-in** reservation may reach (#2062).
///
/// Before this constant the two populations shared one number line and were kept
/// apart only by the order in which they happened to be created. The watermark
/// is `max(reserved) + 1`, so the largest built-in constant decided where
/// dynamic allocation began: `SYN_ACTION_PANEL_VERSION = 2_020_001` put it at
/// `2_020_002`, and 98 runtime-minted generations then walked up through
/// `2_020_099` looking exactly like "the action panel plus k". They had nothing
/// to do with the action panel. Three separate readers — a census, an issue, and
/// a root-cause hunt for `panel_version + 1` arithmetic that does not exist —
/// were all sent to the wrong file by that coincidence.
///
/// Built-in versions are `issue_number * 1000 + n`, so the whole built-in space
/// is bounded by the issue number: reaching this floor would take issue
/// #3,000,000. Everything at or above it is runtime-minted, everything below it
/// is code-declared, and [`reserve_vault_panel_generations`] and
/// [`next_free_generation`] each enforce their own side — so a future built-in
/// bump can no longer silently walk into a range the allocator is minting from,
/// whatever order the two happen in.
///
/// Generations minted below the floor before #2062 keep their owner rows and
/// stay valid; they are superseded by the ordinary retirement path rather than
/// invalidated retroactively.
pub const CALYX_DYNAMIC_PANEL_GENERATION_FLOOR: u32 = 3_000_000_000;

/// Allocator state schema carrying the #2062 retirement ledger.
const SCHEMA_VERSION: u16 = 2;
/// The pre-#2062 schema: identical, minus the retirement ledger.
const SCHEMA_VERSION_PRE_2062: u16 = 1;
const ALLOCATOR_KEY: &[u8] = b"panel-generation-allocator\0v1";
const MAX_OWNERS: usize = 10_000;
const MAX_CAS_ATTEMPTS: usize = 64;
const MAX_PANEL_NAME_BYTES: usize = 128;
const DYNAMIC_OWNER_PREFIX: &str = "dynamic:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelGenerationAllocatorState {
    schema_version: u16,
    next_generation: u32,
    owners: BTreeMap<u32, String>,
    operations: BTreeMap<String, u32>,
    /// Retired generation -> the generation that superseded it (#2062).
    ///
    /// Absent from a v1 row, and absent means "nothing retired", which is
    /// exactly the pre-#2062 truth rather than an assumption about it — so the
    /// upgrade in [`decode_state`] is total. `validate` refuses a v1 row that
    /// carries entries here, so the default can never launder real state.
    #[serde(default)]
    retired: BTreeMap<u32, u32>,
}

impl Default for PanelGenerationAllocatorState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_generation: 1,
            owners: BTreeMap::new(),
            operations: BTreeMap::new(),
            retired: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationAllocation {
    pub panel_name: String,
    pub operation_id: String,
    pub panel_generation: u32,
    pub committed_seq: u64,
    pub existing_identical: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationAllocatorReadback {
    pub schema_version: u16,
    pub next_generation: u32,
    pub owner_count: u64,
    pub operation_count: u64,
    pub owners: BTreeMap<u32, String>,
    /// Retired generation -> superseding generation (#2062).
    ///
    /// A census joining against [`Self::owners`] learns *who* wrote a
    /// generation; this says whether anything still writes there. The pair is
    /// what turns a runtime-minted generation from "unattributable" into either
    /// "the one live derived generation of this panel" or "superseded history,
    /// visible to reclaim".
    pub retired: BTreeMap<u32, u32>,
    pub latest_seq: u64,
}

/// One generation's durable ownership claim (#2062 ask 2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationClaim {
    pub panel_generation: u32,
    /// `builtin:<panel_name>` or `dynamic:<panel_name>:<operation_id>`.
    pub owner: String,
    /// The generation that superseded this one, when it has been retired.
    pub retired_by: Option<u32>,
}

/// The exact retirement one successful derived publish performed (#2062 ask 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationSupersession {
    pub panel_name: String,
    pub successor: u32,
    /// Generations retired by THIS call, named exactly. Never a range.
    pub retired: Vec<u32>,
    /// Generations of this panel already retired before this call. Reported so
    /// an idempotent replay is distinguishable from a no-op that found nothing.
    pub already_retired: Vec<u32>,
    pub committed_seq: u64,
    pub existing_identical: bool,
}

pub fn reserve_vault_panel_generations<C: Clock>(
    vault: &AsterVault<C>,
    reservations: &[(String, u32)],
) -> Result<PanelGenerationAllocatorReadback> {
    validate_reservations(reservations)?;
    for _attempt in 0..MAX_CAS_ATTEMPTS {
        let (mut state, revision) = read_state_revisioned(vault)?;
        let mut changed = false;
        for (panel_name, generation) in reservations {
            let owner = format!("builtin:{panel_name}");
            match state.owners.get(generation) {
                Some(existing) if existing != &owner => {
                    return Err(conflict(format!(
                        "panel generation {generation} is owned by {existing:?}, not {owner:?}"
                    )));
                }
                Some(_) => {}
                None => {
                    ensure_owner_capacity(&state)?;
                    state.owners.insert(*generation, owner);
                    changed = true;
                }
            }
            let next = generation.checked_add(1).ok_or_else(exhausted)?;
            if state.next_generation < next {
                state.next_generation = next;
                changed = true;
            }
        }
        if !changed {
            return read_vault_panel_generation_allocator(vault);
        }
        if let Some(seq) = compare_and_write(vault, revision, &state)? {
            return readback_at(vault, seq, &state);
        }
    }
    Err(conflict(format!(
        "panel generation reservation exceeded {MAX_CAS_ATTEMPTS} atomic retry attempts"
    )))
}

pub fn allocate_vault_panel_generation<C: Clock>(
    vault: &AsterVault<C>,
    panel_name: &str,
    operation_id: &str,
) -> Result<PanelGenerationAllocation> {
    validate_panel_name(panel_name)?;
    validate_operation_id(operation_id)?;
    let operation_key = format!("{panel_name}:{operation_id}");
    for _attempt in 0..MAX_CAS_ATTEMPTS {
        let (mut state, revision) = read_state_revisioned(vault)?;
        if let Some(generation) = state.operations.get(&operation_key).copied() {
            let expected_owner = format!("dynamic:{operation_key}");
            if state.owners.get(&generation) != Some(&expected_owner) {
                return Err(conflict(format!(
                    "operation {operation_key:?} maps to generation {generation}, but its owner row is absent or different"
                )));
            }
            return Ok(PanelGenerationAllocation {
                panel_name: panel_name.to_owned(),
                operation_id: operation_id.to_owned(),
                panel_generation: generation,
                committed_seq: vault.latest_seq(),
                existing_identical: true,
            });
        }
        ensure_owner_capacity(&state)?;
        let generation = next_free_generation(&state)?;
        let next_generation = generation.checked_add(1).ok_or_else(exhausted)?;
        state.next_generation = next_generation;
        state
            .owners
            .insert(generation, format!("dynamic:{operation_key}"));
        state.operations.insert(operation_key.clone(), generation);
        if let Some(seq) = compare_and_write(vault, revision, &state)? {
            let readback = read_state_at(vault, seq)?;
            if readback.operations.get(&operation_key) != Some(&generation)
                || readback.owners.get(&generation) != Some(&format!("dynamic:{operation_key}"))
            {
                return Err(conflict(format!(
                    "allocated panel generation {generation} lacks exact committed readback for operation {operation_key:?}"
                )));
            }
            return Ok(PanelGenerationAllocation {
                panel_name: panel_name.to_owned(),
                operation_id: operation_id.to_owned(),
                panel_generation: generation,
                committed_seq: seq,
                existing_identical: false,
            });
        }
    }
    Err(conflict(format!(
        "panel generation allocation exceeded {MAX_CAS_ATTEMPTS} atomic retry attempts"
    )))
}

/// Retires every generation this panel owns other than `successor`, recording
/// the exact generation that superseded each (#2062 ask 1).
///
/// # Why a successor is required
///
/// This is the same ordering an LSM compaction uses when it retires its input
/// files: the output version is installed first, and only then are the inputs
/// moved onto the obsolete list — never the other way round, and never by
/// guessing which files "look old". Here the successor must already be a
/// committed, owned, un-retired generation of this exact panel, so a retirement
/// can only ever be recorded behind state that is already durable.
///
/// # Why owner identity and not a numeric range
///
/// The generations this retires are selected by the exact owner string
/// `dynamic:<panel_name>:<operation_id>`. A range would be a guess: dynamic
/// generations of *different* panels interleave on one vault-global number line
/// (three publishers share this allocator), so `..successor` would retire other
/// panels' live generations. `builtin:` owners are never eligible.
///
/// # Errors
///
/// Refuses when `successor` is unowned, owned by another panel, or itself
/// retired, and when this panel owns a live generation ABOVE `successor` — that
/// would mean retiring the future, which is a defect in the caller rather than
/// a state to record.
pub fn supersede_vault_panel_generations<C: Clock>(
    vault: &AsterVault<C>,
    panel_name: &str,
    successor: u32,
) -> Result<PanelGenerationSupersession> {
    validate_panel_name(panel_name)?;
    let owner_prefix = format!("{DYNAMIC_OWNER_PREFIX}{panel_name}:");
    for _attempt in 0..MAX_CAS_ATTEMPTS {
        let (mut state, revision) = read_state_revisioned(vault)?;
        match state.owners.get(&successor) {
            None => {
                return Err(conflict(format!(
                    "cannot supersede into panel generation {successor}: it has no owner claim"
                )));
            }
            Some(owner) if !owner.starts_with(&owner_prefix) => {
                return Err(conflict(format!(
                    "panel generation {successor} is owned by {owner:?}, not by dynamic panel {panel_name:?}"
                )));
            }
            Some(_) => {}
        }
        if let Some(retired_by) = state.retired.get(&successor) {
            return Err(conflict(format!(
                "panel generation {successor} was itself retired by {retired_by}; a retired \
                 generation must never become the live one again"
            )));
        }
        let owned: Vec<u32> = state
            .owners
            .iter()
            .filter(|(generation, owner)| {
                **generation != successor && owner.starts_with(&owner_prefix)
            })
            .map(|(generation, _)| *generation)
            .collect();
        if let Some(newer) = owned
            .iter()
            .copied()
            .find(|generation| *generation > successor && !state.retired.contains_key(generation))
        {
            return Err(conflict(format!(
                "panel {panel_name:?} holds live generation {newer} above the successor \
                 {successor}; retiring from an older successor would strand the newer one"
            )));
        }
        let mut retired = Vec::new();
        let mut already_retired = Vec::new();
        for generation in owned {
            if state.retired.contains_key(&generation) {
                already_retired.push(generation);
            } else {
                retired.push(generation);
            }
        }
        if retired.is_empty() {
            return Ok(PanelGenerationSupersession {
                panel_name: panel_name.to_owned(),
                successor,
                retired,
                already_retired,
                committed_seq: vault.latest_seq(),
                existing_identical: true,
            });
        }
        for generation in &retired {
            state.retired.insert(*generation, successor);
        }
        if let Some(seq) = compare_and_write(vault, revision, &state)? {
            let readback = read_state_at(vault, seq)?;
            for generation in &retired {
                if readback.retired.get(generation) != Some(&successor) {
                    return Err(conflict(format!(
                        "retirement of panel generation {generation} under successor {successor} \
                         lacks an exact committed readback"
                    )));
                }
            }
            return Ok(PanelGenerationSupersession {
                panel_name: panel_name.to_owned(),
                successor,
                retired,
                already_retired,
                committed_seq: seq,
                existing_identical: false,
            });
        }
    }
    Err(conflict(format!(
        "panel generation supersession exceeded {MAX_CAS_ATTEMPTS} atomic retry attempts"
    )))
}

/// Reads one generation's durable ownership claim, or `None` when nothing has
/// ever claimed it (#2062 ask 2).
pub fn read_vault_panel_generation_claim<C: Clock>(
    vault: &AsterVault<C>,
    panel_generation: u32,
) -> Result<Option<PanelGenerationClaim>> {
    let state = read_state(vault)?;
    Ok(state
        .owners
        .get(&panel_generation)
        .map(|owner| PanelGenerationClaim {
            panel_generation,
            owner: owner.clone(),
            retired_by: state.retired.get(&panel_generation).copied(),
        }))
}

/// Fails closed when `panel_generation` has no durable ownership claim (#2062
/// ask 2).
///
/// The allocator's `owners` map is the single authority: a built-in generation
/// is claimed by its boot reservation, a runtime generation by its allocation,
/// and nothing else can produce a claim. A writer naming a generation absent
/// from it is naming a generation nothing declared, and the row it is about to
/// write would be attributable to no panel for the life of the vault.
///
/// # Errors
///
/// [`CALYX_PANEL_GENERATION_UNCLAIMED`] when unclaimed.
pub fn ensure_vault_panel_generation_claimed<C: Clock>(
    vault: &AsterVault<C>,
    panel_generation: u32,
) -> Result<PanelGenerationClaim> {
    read_vault_panel_generation_claim(vault, panel_generation)?.ok_or_else(|| CalyxError {
        code: CALYX_PANEL_GENERATION_UNCLAIMED,
        message: format!(
            "panel generation {panel_generation} has no owner claim in the Registry CF allocator; \
             rows written under it would belong to no declared panel"
        ),
        remediation: "reserve the generation for a declared built-in panel, or allocate one \
                      through the panel generation allocator, before writing any row under it",
    })
}

pub fn read_vault_panel_generation_allocator<C: Clock>(
    vault: &AsterVault<C>,
) -> Result<PanelGenerationAllocatorReadback> {
    let state = read_state(vault)?;
    Ok(readback(vault.latest_seq(), state))
}

fn read_state<C: Clock>(vault: &AsterVault<C>) -> Result<PanelGenerationAllocatorState> {
    let state = vault
        .read_cf_latest(ColumnFamily::Registry, ALLOCATOR_KEY)?
        .map(|bytes| decode_state(&bytes))
        .transpose()?
        .unwrap_or_default();
    state.validate()?;
    Ok(state)
}

fn read_state_revisioned<C: Clock>(
    vault: &AsterVault<C>,
) -> Result<(PanelGenerationAllocatorState, Option<[u8; 32]>)> {
    let Some((bytes, revision)) =
        vault.read_cf_latest_revisioned(ColumnFamily::Registry, ALLOCATOR_KEY)?
    else {
        return Ok((PanelGenerationAllocatorState::default(), None));
    };
    Ok((decode_state(&bytes)?, Some(revision)))
}

fn read_state_at<C: Clock>(
    vault: &AsterVault<C>,
    seq: u64,
) -> Result<PanelGenerationAllocatorState> {
    let bytes = vault
        .read_cf_at(seq, ColumnFamily::Registry, ALLOCATOR_KEY)?
        .ok_or_else(|| {
            conflict(format!(
                "panel generation allocator missing at sequence {seq}"
            ))
        })?;
    decode_state(&bytes)
}

fn compare_and_write<C: Clock>(
    vault: &AsterVault<C>,
    revision: Option<[u8; 32]>,
    state: &PanelGenerationAllocatorState,
) -> Result<Option<u64>> {
    state.validate()?;
    let bytes = serde_json::to_vec(state)
        .map_err(|error| invalid(format!("encode panel generation allocator: {error}")))?;
    let outcome = vault.write_cf_batch_if_revision(
        ColumnFamily::Registry,
        ALLOCATOR_KEY,
        revision,
        [(ColumnFamily::Registry, ALLOCATOR_KEY.to_vec(), bytes)],
    )?;
    Ok(outcome.applied.then_some(outcome.seq))
}

fn readback_at<C: Clock>(
    vault: &AsterVault<C>,
    seq: u64,
    expected: &PanelGenerationAllocatorState,
) -> Result<PanelGenerationAllocatorReadback> {
    let actual = read_state_at(vault, seq)?;
    if &actual != expected {
        return Err(conflict(format!(
            "panel generation allocator readback differs at committed sequence {seq}"
        )));
    }
    Ok(readback(seq, actual))
}

fn readback(
    latest_seq: u64,
    state: PanelGenerationAllocatorState,
) -> PanelGenerationAllocatorReadback {
    PanelGenerationAllocatorReadback {
        schema_version: state.schema_version,
        next_generation: state.next_generation,
        owner_count: state.owners.len() as u64,
        operation_count: state.operations.len() as u64,
        owners: state.owners,
        retired: state.retired,
        latest_seq,
    }
}

impl PanelGenerationAllocatorState {
    fn validate(&self) -> Result<()> {
        if !matches!(
            self.schema_version,
            SCHEMA_VERSION | SCHEMA_VERSION_PRE_2062
        ) || self.next_generation == 0
        {
            return Err(invalid(format!(
                "panel generation allocator schema={} next_generation={} is invalid",
                self.schema_version, self.next_generation
            )));
        }
        if self.schema_version == SCHEMA_VERSION_PRE_2062 && !self.retired.is_empty() {
            return Err(invalid(format!(
                "panel generation allocator claims schema {SCHEMA_VERSION_PRE_2062} while \
                 carrying {} retirement record(s); the retirement ledger only exists at schema \
                 {SCHEMA_VERSION}",
                self.retired.len()
            )));
        }
        for (generation, superseded_by) in &self.retired {
            if !self.owners.contains_key(generation) {
                return Err(invalid(format!(
                    "panel generation allocator retires unowned generation {generation}"
                )));
            }
            if superseded_by == generation || !self.owners.contains_key(superseded_by) {
                return Err(invalid(format!(
                    "panel generation {generation} names invalid successor {superseded_by}"
                )));
            }
        }
        if self.owners.len() > MAX_OWNERS || self.operations.len() > MAX_OWNERS {
            return Err(exhausted());
        }
        for (generation, owner) in &self.owners {
            if *generation == 0 || owner.is_empty() {
                return Err(invalid(format!(
                    "panel generation allocator contains invalid owner generation={generation} owner={owner:?}"
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for (operation, generation) in &self.operations {
            if operation.is_empty() || !seen.insert(*generation) {
                return Err(invalid(format!(
                    "panel generation allocator contains invalid or duplicate operation {operation:?} generation={generation}"
                )));
            }
            let Some(owner) = self.owners.get(generation) else {
                return Err(invalid(format!(
                    "panel generation operation {operation:?} points to unowned generation {generation}"
                )));
            };
            if owner != &format!("dynamic:{operation}") {
                return Err(invalid(format!(
                    "panel generation operation {operation:?} owner mismatch: {owner:?}"
                )));
            }
        }
        Ok(())
    }
}

fn validate_reservations(reservations: &[(String, u32)]) -> Result<()> {
    let mut generations = BTreeMap::new();
    for (panel_name, generation) in reservations {
        validate_panel_name(panel_name)?;
        if *generation == 0 {
            return Err(invalid(format!(
                "panel {panel_name:?} reserves zero generation"
            )));
        }
        // #2062: the built-in and dynamic ranges are disjoint by construction,
        // enforced from both sides. This is the built-in side; a reservation
        // that reached the dynamic floor would put a code-declared constant on
        // the same number line the allocator mints from, which is precisely the
        // coincidence that made 98 runtime generations read as "the action
        // panel plus k".
        if *generation >= CALYX_DYNAMIC_PANEL_GENERATION_FLOOR {
            return Err(invalid(format!(
                "panel {panel_name:?} reserves generation {generation} at or above the dynamic \
                 allocator floor {CALYX_DYNAMIC_PANEL_GENERATION_FLOOR}; built-in generations \
                 must stay below it so the two ranges can never collide"
            )));
        }
        if let Some(existing) = generations.insert(*generation, panel_name.as_str())
            && existing != panel_name
        {
            return Err(conflict(format!(
                "generation {generation} is requested by both {existing:?} and {panel_name:?}"
            )));
        }
    }
    Ok(())
}

fn validate_panel_name(panel_name: &str) -> Result<()> {
    if panel_name.is_empty()
        || panel_name.len() > MAX_PANEL_NAME_BYTES
        || !panel_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid(format!(
            "panel name {panel_name:?} must be 1..={MAX_PANEL_NAME_BYTES} lowercase ASCII letters, digits, or hyphens"
        )));
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.len() != 64
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "operation_id {operation_id:?} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn ensure_owner_capacity(state: &PanelGenerationAllocatorState) -> Result<()> {
    if state.owners.len() >= MAX_OWNERS || state.operations.len() >= MAX_OWNERS {
        return Err(exhausted());
    }
    Ok(())
}

/// The dynamic side of the #2062 range split.
///
/// The watermark is still honoured — it is what keeps allocation monotone
/// within the dynamic range — but it can no longer decide where that range
/// *starts*. Raising it is a built-in reservation's business and now has no
/// effect on what a dynamic allocation mints, because the floor dominates until
/// the dynamic range itself has advanced past it.
fn next_free_generation(state: &PanelGenerationAllocatorState) -> Result<u32> {
    let mut candidate = state
        .next_generation
        .max(CALYX_DYNAMIC_PANEL_GENERATION_FLOOR);
    while state.owners.contains_key(&candidate) {
        candidate = candidate.checked_add(1).ok_or_else(exhausted)?;
    }
    Ok(candidate)
}

fn decode_state(bytes: &[u8]) -> Result<PanelGenerationAllocatorState> {
    let mut state: PanelGenerationAllocatorState = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("decode panel generation allocator: {error}")))?;
    state.validate()?;
    // #2062: a v1 row predates the retirement ledger, and an absent ledger says
    // exactly what was true before it existed — nothing had been retired. The
    // upgrade therefore carries no assumption; `validate` above has already
    // refused a v1 row that claims otherwise.
    if state.schema_version == SCHEMA_VERSION_PRE_2062 {
        state.schema_version = SCHEMA_VERSION;
    }
    Ok(state)
}

fn invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_INVALID,
        message: message.into(),
        remediation: "repair the exact Registry CF allocator row before changing any panel",
    }
}

fn conflict(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_CONFLICT,
        message: message.into(),
        remediation: "retry with the same operation identity or inspect conflicting Registry CF ownership",
    }
}

fn exhausted() -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_EXHAUSTED,
        message: "panel generation allocator is exhausted".to_owned(),
        remediation: "archive retired generations into a new explicitly versioned allocator format",
    }
}
