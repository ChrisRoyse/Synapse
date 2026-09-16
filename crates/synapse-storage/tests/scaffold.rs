use std::{
    error::Error,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_storage::{Db, decode_json, encode_json};

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct SamplePayload {
    id: String,
    seq: u32,
}

#[test]
fn db_open_creates_directory() -> Result<(), Box<dyn Error>> {
    let path = unique_temp_path("open")?.join("db");
    println!(
        "regression_state=storage_db_open before_path={} before_exists={}",
        path.display(),
        path.exists()
    );
    let db = Db::open(&path, 7)?;
    println!(
        "regression_state=storage_db_open after_path={} after_exists={} after_schema_version={}",
        db.path.display(),
        db.path.exists(),
        db.schema_version
    );
    assert!(db.path.is_dir());
    assert_eq!(db.schema_version, 7);
    drop(db);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn db_open_rejects_file_path() -> Result<(), Box<dyn Error>> {
    let root = unique_temp_path("file")?;
    fs::create_dir_all(&root)?;
    let path = root.join("db-file");
    fs::write(&path, b"not a directory")?;
    println!(
        "regression_state=storage_db_open edge=file_path before_path={} before_is_file={}",
        path.display(),
        path.is_file()
    );
    let error = match Db::open(&path, 7) {
        Ok(db) => panic!("Db::open unexpectedly accepted file path: {db:?}"),
        Err(error) => error,
    };
    println!(
        "regression_state=storage_db_open edge=file_path after_code={} after_path_still_file={}",
        error.code(),
        path.is_file()
    );
    assert_eq!(error.code(), error_codes::STORAGE_OPEN_FAILED);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn json_codecs_round_trip_and_reject_invalid() -> Result<(), Box<dyn Error>> {
    let before = SamplePayload {
        id: "synthetic-storage-payload".to_owned(),
        seq: 42,
    };
    println!("regression_state=storage_codec before_payload={before:?}");
    let bytes = encode_json(&before)?;
    let after: SamplePayload = decode_json(&bytes)?;
    println!(
        "regression_state=storage_codec after_payload={after:?} after_bytes={}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(after, before);

    let invalid = br#"{"id":"missing seq"}"#;
    println!(
        "regression_state=storage_codec edge=invalid before_bytes={}",
        String::from_utf8_lossy(invalid)
    );
    let error = match decode_json::<SamplePayload>(invalid) {
        Ok(value) => panic!("decode_json unexpectedly accepted invalid payload: {value:?}"),
        Err(error) => error,
    };
    println!(
        "regression_state=storage_codec edge=invalid after_code={}",
        error.code()
    );
    assert_eq!(error.code(), error_codes::STORAGE_READ_FAILED);
    Ok(())
}

fn unique_temp_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(std::env::temp_dir().join(format!(
        "synapse-storage-{name}-{}-{nanos}",
        std::process::id()
    )))
}
