use super::*;
use std::{error::Error, fs};

use rocksdb::DEFAULT_COLUMN_FAMILY_NAME;
use synapse_core::error_codes;

const TEST_SCHEMA_VERSION: u32 = 7;
const DURABILITY_KEY: &[u8] = b"restart-durability-key";
const DURABILITY_VALUE: &[u8] = b"restart-durability-value";

#[test]
fn open_fresh_db_creates_all_prd_cfs_and_schema_sentinel() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    println!(
        "regression_state=db.list_cf edge=fresh before_path={} before={:?}",
        path.display(),
        list_cf_for(&path)
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let sentinel = db.inner.get(SCHEMA_VERSION_KEY)?;
    let expected_schema = TEST_SCHEMA_VERSION.to_be_bytes();
    println!(
        "regression_state=db.list_cf edge=fresh after_handles={handles:?} after_sentinel={:?} observed=count:{}",
        sentinel.as_deref(),
        handles.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(sentinel.as_deref(), Some(expected_schema.as_slice()));
    drop(db);

    let physical = sorted_list_cf(&path)?;
    println!("regression_state=db.list_cf edge=fresh physical_after_drop={physical:?}");
    assert_eq!(physical, sorted_physical_cfs());
    Ok(())
}

#[test]
fn opens_database_created_with_pre_timeline_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the historical 11-CF layout (no CF_TIMELINE, and therefore
    // none of its derived, routine-state, or agent-event CFs either) exactly
    // as a pre-ADR binary would have left it.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| {
            *name != cf::CF_TIMELINE
                && *name != cf::CF_EPISODES
                && *name != cf::CF_ROUTINES
                && *name != cf::CF_ROUTINE_STATE
                && *name != cf::CF_AGENT_EVENTS
                && *name != cf::CF_AGENT_TRANSCRIPTS
        })
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let events = legacy
            .cf_handle(cf::CF_EVENTS)
            .ok_or("legacy CF_EVENTS handle missing")?;
        legacy.put_cf(&events, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&events)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_timeline before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_TIMELINE.to_owned()),
        "precondition: legacy layout must not contain CF_TIMELINE"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_EVENTS)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(cf::CF_TIMELINE, vec![(b"migrate".to_vec(), b"{}".to_vec())])?;
    db.flush()?;
    let timeline_rows = db.scan_cf(cf::CF_TIMELINE)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_timeline after={after:?} preserved={:?} timeline_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        timeline_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(timeline_rows.len(), 1);
    assert!(after.contains(&cf::CF_TIMELINE.to_owned()));
    Ok(())
}

#[test]
fn opens_database_created_with_pre_episodes_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the 12-CF layout (no CF_EPISODES, and therefore no
    // CF_ROUTINES, CF_ROUTINE_STATE, or CF_AGENT_EVENTS) exactly as a
    // pre-#846 binary would have left it on disk.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| {
            *name != cf::CF_EPISODES
                && *name != cf::CF_ROUTINES
                && *name != cf::CF_ROUTINE_STATE
                && *name != cf::CF_AGENT_EVENTS
                && *name != cf::CF_AGENT_TRANSCRIPTS
        })
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let timeline = legacy
            .cf_handle(cf::CF_TIMELINE)
            .ok_or("legacy CF_TIMELINE handle missing")?;
        legacy.put_cf(&timeline, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&timeline)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_episodes before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_EPISODES.to_owned()),
        "precondition: legacy layout must not contain CF_EPISODES"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_TIMELINE)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(cf::CF_EPISODES, vec![(b"migrate".to_vec(), b"{}".to_vec())])?;
    db.flush()?;
    let episode_rows = db.scan_cf(cf::CF_EPISODES)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_episodes after={after:?} preserved={:?} episode_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        episode_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(episode_rows.len(), 1);
    assert!(after.contains(&cf::CF_EPISODES.to_owned()));
    Ok(())
}

#[test]
fn opens_database_created_with_pre_routines_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the 13-CF layout (everything except CF_ROUTINES,
    // CF_ROUTINE_STATE, and CF_AGENT_EVENTS) exactly as a pre-#848 binary
    // would have left it on disk.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| {
            *name != cf::CF_ROUTINES
                && *name != cf::CF_ROUTINE_STATE
                && *name != cf::CF_AGENT_EVENTS
                && *name != cf::CF_AGENT_TRANSCRIPTS
        })
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let episodes = legacy
            .cf_handle(cf::CF_EPISODES)
            .ok_or("legacy CF_EPISODES handle missing")?;
        legacy.put_cf(&episodes, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&episodes)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_routines before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_ROUTINES.to_owned()),
        "precondition: legacy layout must not contain CF_ROUTINES"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_EPISODES)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(
        cf::CF_ROUTINES,
        vec![(b"rt1-0123456789abcdef".to_vec(), b"{}".to_vec())],
    )?;
    db.flush()?;
    let routine_rows = db.scan_cf(cf::CF_ROUTINES)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_routines after={after:?} preserved={:?} routine_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        routine_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(routine_rows.len(), 1);
    assert!(after.contains(&cf::CF_ROUTINES.to_owned()));
    Ok(())
}

#[test]
fn opens_database_created_with_pre_routine_state_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the 14-CF layout (everything except CF_ROUTINE_STATE and
    // CF_AGENT_EVENTS) exactly as a pre-#849 binary would have left it on
    // disk.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| {
            *name != cf::CF_ROUTINE_STATE
                && *name != cf::CF_AGENT_EVENTS
                && *name != cf::CF_AGENT_TRANSCRIPTS
        })
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let routines = legacy
            .cf_handle(cf::CF_ROUTINES)
            .ok_or("legacy CF_ROUTINES handle missing")?;
        legacy.put_cf(&routines, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&routines)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_routine_state before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_ROUTINE_STATE.to_owned()),
        "precondition: legacy layout must not contain CF_ROUTINE_STATE"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_ROUTINES)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(
        cf::CF_ROUTINE_STATE,
        vec![(b"rt1-0123456789abcdef".to_vec(), b"{}".to_vec())],
    )?;
    db.flush()?;
    let state_rows = db.scan_cf(cf::CF_ROUTINE_STATE)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_routine_state after={after:?} preserved={:?} \
         state_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        state_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(state_rows.len(), 1);
    assert!(after.contains(&cf::CF_ROUTINE_STATE.to_owned()));
    Ok(())
}

#[test]
fn opens_database_created_with_pre_agent_events_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the 15-CF layout (everything except CF_AGENT_EVENTS and
    // CF_AGENT_TRANSCRIPTS) exactly as a pre-#897 binary would have left it
    // on disk.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| *name != cf::CF_AGENT_EVENTS && *name != cf::CF_AGENT_TRANSCRIPTS)
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let routine_state = legacy
            .cf_handle(cf::CF_ROUTINE_STATE)
            .ok_or("legacy CF_ROUTINE_STATE handle missing")?;
        legacy.put_cf(&routine_state, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&routine_state)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_agent_events before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_AGENT_EVENTS.to_owned()),
        "precondition: legacy layout must not contain CF_AGENT_EVENTS"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_ROUTINE_STATE)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(
        cf::CF_AGENT_EVENTS,
        vec![(
            agent_events::agent_event_key(1, 0),
            br#"{"ts_ns":1}"#.to_vec(),
        )],
    )?;
    db.flush()?;
    let event_rows = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_agent_events after={after:?} preserved={:?} \
         event_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        event_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(event_rows.len(), 1);
    assert!(after.contains(&cf::CF_AGENT_EVENTS.to_owned()));
    Ok(())
}

#[test]
fn opens_database_created_with_pre_agent_transcripts_layout() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // Recreate the 16-CF layout (everything except CF_AGENT_TRANSCRIPTS)
    // exactly as a pre-#900 binary would have left it on disk.
    let legacy_cfs: Vec<&str> = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| *name != cf::CF_AGENT_TRANSCRIPTS)
        .collect();
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let legacy = DB::open_cf(&options, &path, &legacy_cfs)?;
        legacy.put(SCHEMA_VERSION_KEY, TEST_SCHEMA_VERSION.to_be_bytes())?;
        let agent_events = legacy
            .cf_handle(cf::CF_AGENT_EVENTS)
            .ok_or("legacy CF_AGENT_EVENTS handle missing")?;
        legacy.put_cf(&agent_events, DURABILITY_KEY, DURABILITY_VALUE)?;
        legacy.flush_cf(&agent_events)?;
    }
    let before = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_agent_transcripts before={before:?} before_count={}",
        before.len()
    );
    assert!(
        !before.contains(&cf::CF_AGENT_TRANSCRIPTS.to_owned()),
        "precondition: legacy layout must not contain CF_AGENT_TRANSCRIPTS"
    );

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let handles = existing_prd_handles(&db);
    let preserved = db
        .inner
        .cf_handle(cf::CF_AGENT_EVENTS)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    db.put_batch(
        cf::CF_AGENT_TRANSCRIPTS,
        vec![(
            agent_transcripts::agent_transcript_key("agent-spawn-migrate", 1),
            br#"{"ts_ns":1}"#.to_vec(),
        )],
    )?;
    db.flush()?;
    let transcript_rows = db.scan_cf(cf::CF_AGENT_TRANSCRIPTS)?;
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=pre_agent_transcripts after={after:?} preserved={:?} \
         transcript_rows={} observed=additive_open:ok",
        preserved.as_deref(),
        transcript_rows.len()
    );
    assert_eq!(handles, sorted_prd_cfs());
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(transcript_rows.len(), 1);
    assert!(after.contains(&cf::CF_AGENT_TRANSCRIPTS.to_owned()));
    Ok(())
}

#[test]
fn opens_database_holding_a_future_unknown_cf() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");

    // A newer binary created the database, added a CF this binary does not
    // know, and wrote rows into it (the 2026-06-12 rollback incident shape:
    // a pre-CF_ROUTINES daemon died at startup on a post-CF_ROUTINES DB).
    {
        let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
        db.put_batch(
            cf::CF_EVENTS,
            vec![(DURABILITY_KEY.to_vec(), DURABILITY_VALUE.to_vec())],
        )?;
        db.flush()?;
    }
    {
        let mut options = Options::default();
        options.create_if_missing(false);
        options.create_missing_column_families(true);
        let mut all: Vec<&str> = cf::ALL_COLUMN_FAMILIES.to_vec();
        all.push("CF_FROM_THE_FUTURE");
        let future = DB::open_cf(&options, &path, &all)?;
        let handle = future
            .cf_handle("CF_FROM_THE_FUTURE")
            .ok_or("future CF handle missing")?;
        future.put_cf(&handle, b"future-key", b"future-value")?;
        future.flush_cf(&handle)?;
    }
    let before = sorted_list_cf(&path)?;
    println!("regression_state=db.list_cf edge=future_cf before={before:?}");
    assert!(before.contains(&"CF_FROM_THE_FUTURE".to_owned()));

    // This binary must open the database anyway, preserve both its own rows
    // and the unknown CF's rows, and leave the unknown CF physically intact.
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let preserved = db
        .inner
        .cf_handle(cf::CF_EVENTS)
        .and_then(|handle| db.inner.get_cf(&handle, DURABILITY_KEY).transpose())
        .transpose()?;
    let future_value = db
        .inner
        .cf_handle("CF_FROM_THE_FUTURE")
        .and_then(|handle| db.inner.get_cf(&handle, b"future-key").transpose())
        .transpose()?;
    assert_eq!(preserved.as_deref(), Some(DURABILITY_VALUE));
    assert_eq!(future_value.as_deref(), Some(b"future-value".as_slice()));
    drop(db);

    let after = sorted_list_cf(&path)?;
    println!(
        "regression_state=db.list_cf edge=future_cf after={after:?} observed=tolerant_open:ok"
    );
    assert!(after.contains(&"CF_FROM_THE_FUTURE".to_owned()));
    Ok(())
}

#[test]
fn mismatched_schema_errors_then_wipe_retry_succeeds() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let db = Db::open(&path, 1)?;
    let before = db.inner.get(SCHEMA_VERSION_KEY)?;
    drop(db);
    println!(
        "regression_state=db.schema edge=mismatch before_path={} before_sentinel={before:?}",
        path.display()
    );

    let error = match Db::open(&path, TEST_SCHEMA_VERSION) {
        Ok(db) => panic!("Db::open unexpectedly accepted mismatched schema: {db:?}"),
        Err(error) => error,
    };
    println!(
        "regression_state=db.schema edge=mismatch after_code={} after_db_exists={}",
        error.code(),
        path.exists()
    );
    assert_eq!(error.code(), error_codes::STORAGE_SCHEMA_MISMATCH);

    fs::remove_dir_all(&path)?;
    println!(
        "regression_state=db.schema edge=mismatch after_wipe_exists={}",
        path.exists()
    );
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let after = db.inner.get(SCHEMA_VERSION_KEY)?;
    let expected_schema = TEST_SCHEMA_VERSION.to_be_bytes();
    println!(
        "regression_state=db.schema edge=mismatch retry_sentinel={after:?} observed=schema_version:{}",
        db.schema_version
    );
    assert_eq!(after.as_deref(), Some(expected_schema.as_slice()));
    Ok(())
}

#[test]
fn process_restart_reads_persisted_key() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    {
        let cf = db
            .inner
            .cf_handle(cf::CF_KV)
            .ok_or("CF_KV handle missing after open")?;
        println!(
            "regression_state=db.restart edge=durability before_key={:?} before_value={:?}",
            String::from_utf8_lossy(DURABILITY_KEY),
            db.inner.get_cf(&cf, DURABILITY_KEY)?
        );
        db.inner.put_cf(&cf, DURABILITY_KEY, DURABILITY_VALUE)?;
        db.inner.flush_cf(&cf)?;
    }
    drop(db);

    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let cf = db
        .inner
        .cf_handle(cf::CF_KV)
        .ok_or("CF_KV handle missing after reopen")?;
    let after = db.inner.get_cf(&cf, DURABILITY_KEY)?;
    println!(
        "regression_state=db.restart edge=durability after_value={after:?} observed={:?}",
        Some(DURABILITY_VALUE)
    );
    assert_eq!(after.as_deref(), Some(DURABILITY_VALUE));
    Ok(())
}

#[test]
fn file_path_open_fails_with_storage_open_failed() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db-file");
    fs::write(&path, b"not a directory")?;
    println!(
        "regression_state=db.list_cf edge=file_path before_path={} before_is_file={}",
        path.display(),
        path.is_file()
    );
    let error = match Db::open(&path, TEST_SCHEMA_VERSION) {
        Ok(db) => panic!("Db::open unexpectedly accepted file path: {db:?}"),
        Err(error) => error,
    };
    println!(
        "regression_state=db.list_cf edge=file_path after_code={} after_detail={:?} after_is_file={}",
        error.code(),
        open_error_detail(&error),
        path.is_file()
    );
    assert_eq!(error.code(), error_codes::STORAGE_OPEN_FAILED);
    Ok(())
}

#[cfg(windows)]
#[test]
fn non_writable_path_returns_storage_open_failed() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("locked-db");
    fs::create_dir_all(&path)?;
    let _guard = deny_current_user_write(&path)?;
    println!(
        "regression_state=db.list_cf edge=permission before_path={} before={:?}",
        path.display(),
        list_cf_for(&path)
    );

    let error = match Db::open(&path, TEST_SCHEMA_VERSION) {
        Ok(db) => panic!("Db::open unexpectedly accepted non-writable path: {db:?}"),
        Err(error) => error,
    };
    println!(
        "regression_state=db.list_cf edge=permission after_code={} after_detail={:?} after_exists={}",
        error.code(),
        open_error_detail(&error),
        path.exists()
    );
    assert_eq!(error.code(), error_codes::STORAGE_OPEN_FAILED);
    assert!(open_error_detail(&error).contains("Access is denied"));
    Ok(())
}

fn existing_prd_handles(db: &Db) -> Vec<&'static str> {
    let mut names = cf::ALL_COLUMN_FAMILIES
        .into_iter()
        .filter(|name| db.inner.cf_handle(name).is_some())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

fn sorted_prd_cfs() -> Vec<&'static str> {
    let mut names = cf::ALL_COLUMN_FAMILIES.to_vec();
    names.sort_unstable();
    names
}

fn sorted_physical_cfs() -> Vec<String> {
    let mut names = sorted_prd_cfs()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    names.push(DEFAULT_COLUMN_FAMILY_NAME.to_owned());
    names.sort_unstable();
    names
}

fn sorted_list_cf(path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut cfs = DB::list_cf(&Options::default(), path)?;
    cfs.sort_unstable();
    Ok(cfs)
}

fn list_cf_for(path: &Path) -> Result<Vec<String>, String> {
    sorted_list_cf(path).map_err(|error| error.to_string())
}

fn open_error_detail(error: &StorageError) -> &str {
    match error {
        StorageError::OpenFailed { detail, .. } => detail,
        _ => "",
    }
}

#[cfg(windows)]
struct DenyWriteGuard {
    path: std::path::PathBuf,
    principal: String,
}

#[cfg(windows)]
impl Drop for DenyWriteGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("icacls")
            .arg(&self.path)
            .args(["/remove:d", &self.principal])
            .output();
    }
}

#[cfg(windows)]
fn deny_current_user_write(path: &Path) -> Result<DenyWriteGuard, Box<dyn Error>> {
    let domain = std::env::var("USERDOMAIN")?;
    let username = std::env::var("USERNAME")?;
    let principal = format!(r"{domain}\{username}");
    let deny = format!("{principal}:(W)");
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(["/deny", &deny])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "icacls deny failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(DenyWriteGuard {
        path: path.to_path_buf(),
        principal,
    })
}
