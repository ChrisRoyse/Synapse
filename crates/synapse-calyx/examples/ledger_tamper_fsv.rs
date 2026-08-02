//! Manual FSV for #1679's second acceptance clause: is a deliberate byte-flip
//! in a **sealed** ledger segment actually detected?
//!
//! ## Why the positive result alone proves nothing
//!
//! `audit verify_chain` reports `intact` over the live vault's 124,011 entries.
//! A verifier that unconditionally returned `intact` would report exactly the
//! same thing. #1679's acceptance says so explicitly — "verify_chain green over
//! live history; **deliberate byte-flip in a sealed segment detected**" — and
//! only the first half has been run.
//!
//! `verify_ledger_chain`'s own documentation makes the claim under test:
//!
//! > a single flipped byte anywhere in the requested window yields a `Broken`
//! > or `Corrupt` verdict rather than a silent pass
//!
//! ## Construction
//!
//! Never against the live vault. This runs on a **copy**, opened read-only, and
//! the run is three measurements with the expectation stated before each:
//!
//! 1. verify the untouched copy      -> expect `intact` (the copy is faithful)
//! 2. flip exactly one bit in one Ledger CF SST -> expect `broken`/`corrupt`
//! 3. restore the original byte      -> expect `intact` again
//!
//! Step 3 is what makes step 2 mean something: it proves the verdict tracked the
//! flipped bit specifically, rather than the copy having been broken all along
//! or the act of opening it having damaged something.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example ledger_tamper_fsv -- <vault-copy-dir>`

use std::error::Error;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;
use std::str::FromStr as _;

/// Opens the copy read-only.
///
/// `restore_ledger_hook` MUST stay on. With it off the head anchor is never
/// loaded, so `head_height` is 0, the verified range is `0..0`, and
/// `verify_ledger_chain` returns `Intact { count: 0 }` — a pass that verified
/// nothing. The first version of this harness did exactly that and reported a
/// green step 1 over an empty chain.
///
/// `restore_mvcc_rows` MUST also stay on, for the same reason. The chain walk
/// resolves its head anchor and scans `ColumnFamily::RawCommitment` through the
/// MVCC view, so without a restore the vault reads as empty and the verify is
/// again vacuous. The second version of this harness set it to `false` as a
/// "cheap open" and got `head_height = 0` a second time.
///
/// The cost is a real full restore of the copy on every open, which is the
/// price of the verify meaning anything.
fn open_readonly(
    dir: &Path,
    vault_id: VaultId,
    salt: Vec<u8>,
) -> Result<AsterVault, Box<dyn Error>> {
    let options = VaultOptions {
        // NOT read_only. `open.rs` sets `durable: None` for a read-only
        // handle, and `head_anchor()` returns None when durable is None -- so
        // `verify_ledger_chain` on a read-only vault verifies an EMPTY chain
        // and returns `Intact { count: 0 }`. That is a fail-open in a
        // verification primitive and is filed separately; this harness opens
        // the disposable backup writable so the chain is actually there.
        read_only: false,
        restore_mvcc_rows: true,
        restore_ledger_hook: true,
        eager_router_lookup_on_open: false,
        ..VaultOptions::default()
    };
    Ok(AsterVault::open(dir, vault_id, salt, options)?)
}

/// Every Ledger CF SST in the copy, largest first — the largest is the one most
/// likely to hold sealed entries rather than a nearly-empty tail.
fn ledger_ssts(vault_dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let ledger_dir = vault_dir.join("cf").join("ledger");
    let Ok(entries) = std::fs::read_dir(&ledger_dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "sst")
            && let Ok(meta) = entry.metadata()
        {
            found.push((path, meta.len()));
        }
    }
    found.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    found
}

/// Runs the chain verify and reduces it to `(verdict, head_height, intact)`.
///
/// A structured error is reported as **not intact** rather than propagated: a
/// tamper severe enough to fail the read is still a detection, and treating it
/// as a harness failure would throw away the result the test exists to observe.
/// Note this diverges from `verify_ledger_chain`'s documented contract, which
/// says a detected tamper is "a normal `Ok(Broken | Corrupt)` verdict, never an
/// `Err`" — see the report for that discrepancy.
fn verdict_of(vault: &AsterVault) -> (String, u64, bool) {
    match vault.verify_ledger_chain(None) {
        Ok(verification) => {
            let verdict = format!("{:?}", verification.result);
            let intact = verdict.to_lowercase().contains("intact");
            (verdict, verification.head_height, intact)
        }
        Err(error) => (format!("Err[{}]: {}", error.code, error.message), 0, false),
    }
}

/// Opens the copy and verifies, reporting an open failure as a detection too.
fn open_and_verify(dir: &Path, vault_id: VaultId, salt: Vec<u8>) -> (String, u64, bool) {
    match open_readonly(dir, vault_id, salt) {
        Ok(vault) => verdict_of(&vault),
        Err(error) => (format!("open refused: {error}"), 0, false),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ledger_tamper_fsv <vault-copy-dir>  (NEVER the live vault)")?;

    // Refuse to run against the daemon's own vault. A tamper test that can be
    // pointed at production by a typo is not a safe instrument.
    let live = PathBuf::from(std::env::var("LOCALAPPDATA")?)
        .join("synapse")
        .join("db-daemon");
    if dir.canonicalize().ok() == live.canonicalize().ok() {
        return Err("refusing to tamper with the live daemon vault".into());
    }

    let identity_path = dir.join("vault-identity.json");
    let identity = std::fs::read_to_string(&identity_path)?;
    let vault_id_hex = identity
        .split("\"vault_id\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .ok_or("could not read vault_id from vault-identity.json")?
        .to_owned();
    let vault_id = VaultId::from_str(&vault_id_hex)?;
    let salt = std::fs::read(dir.parent().ok_or("no parent")?.join("machine-salt.bin"))?;

    println!("ledger_tamper_fsv  (#1679 acceptance clause 2)");

    // --- 0. the read-only handle must REFUSE, not pass vacuously (#1956) ----
    // Before the fix this returned `Intact { count: 0 }` on a vault holding
    // 125,735 chain entries. A verification primitive that reports success
    // having checked nothing is worse than one that errors, and a read-only
    // handle is precisely what an operator uses to check a restored backup.
    {
        let options = VaultOptions {
            read_only: true,
            restore_mvcc_rows: false,
            restore_ledger_hook: false,
            ..VaultOptions::default()
        };
        match AsterVault::open(&dir, vault_id, salt.clone(), options)
            .and_then(|vault| vault.verify_ledger_chain(None))
        {
            Ok(verification) => {
                println!(
                    "  read-only verify returned Ok({:?}) head_height={}  <-- FAIL-OPEN",
                    verification.result, verification.head_height
                );
                return Err(
                    "read-only handle reported a chain verdict instead of refusing (#1956)".into(),
                );
            }
            Err(error) if error.code == "CALYX_ASTER_LEDGER_VERIFY_UNAVAILABLE" => {
                println!(
                    "
=== 0. read-only handle refuses (#1956) ==="
                );
                println!("  Err[{}] as required", error.code);
            }
            Err(error) => {
                println!("  read-only verify failed for another reason: {error}");
                return Err("read-only refusal did not carry the expected code".into());
            }
        }
    }
    println!("vault copy = {}", dir.display());
    println!("vault_id   = {vault_id_hex}");

    // --- 1. the untouched copy must verify --------------------------------
    println!("\n=== 1. untouched copy must verify intact ===");
    let (verdict, height, intact_before) = open_and_verify(&dir, vault_id, salt.clone());
    println!("  verdict = {verdict}   head_height = {height}");
    if !intact_before {
        return Err(
            "the untouched copy does not verify; the copy is not faithful and nothing below would mean anything"
                .into(),
        );
    }
    // A chain of height 0 verifies trivially. Without this the whole run is
    // vacuous and still prints PASS.
    if height == 0 {
        return Err(
            "the copy verified over an EMPTY chain (head_height=0); this proves nothing and the harness is misconfigured"
                .into(),
        );
    }

    // --- 2. flip exactly one bit in a sealed Ledger SST --------------------
    println!("\n=== 2. flip one bit in a Ledger CF SST ===");
    let ssts = ledger_ssts(&dir);
    let (target, size) = ssts
        .first()
        .cloned()
        .ok_or("no Ledger CF SST found in the copy; nothing sealed to tamper with")?;
    // Middle of the largest file: past any header, inside real sealed content,
    // and far from the unsealed tail that would prove nothing.
    let offset = size / 2;
    println!("  target = {}", target.display());
    println!("  size = {size} bytes, flipping the low bit at offset {offset}");

    let original = {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)?;
        let original = byte[0];
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&[original ^ 0x01])?;
        file.sync_all()?;
        original
    };
    println!("  byte {original:#04x} -> {:#04x}", original ^ 0x01);

    let (tampered_verdict, tampered_height, intact_after) =
        open_and_verify(&dir, vault_id, salt.clone());
    println!("  verdict = {tampered_verdict}   head_height = {tampered_height}");
    let detected = !intact_after;
    println!(
        "  DETECTED = {detected} {}",
        if detected { "" } else { "<-- SILENT PASS" }
    );

    // --- 3. restore, and the verdict must go back to intact ----------------
    // Without this, a `broken` in step 2 could mean "this copy was always
    // broken" rather than "the flipped bit was detected".
    println!("\n=== 3. restore the original byte; verdict must return to intact ===");
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&[original])?;
        file.sync_all()?;
    }
    let (restored_verdict, restored_height, intact_restored) =
        open_and_verify(&dir, vault_id, salt);
    let intact_restored = intact_restored && restored_height > 0;
    println!("  verdict = {restored_verdict}   head_height = {restored_height}");

    println!("\n--- VERDICT ---");
    println!("  untouched intact  : {intact_before}");
    println!("  tamper detected   : {detected}");
    println!("  restored intact   : {intact_restored}");
    if intact_before && detected && intact_restored {
        println!("  PASS: the chain verifier detects a single flipped bit and tracks it");
        Ok(())
    } else if !detected {
        Err("SILENT PASS: a flipped bit in a sealed Ledger SST was not detected".into())
    } else {
        Err(
            "the verdict did not return to intact after restore; the flip is not what was detected"
                .into(),
        )
    }
}
