//! Offline column-family readback utility.
//!
//! Its output is supporting storage evidence only; manual FSV remains separate.
//!
//! Opens an existing Synapse storage backend strictly for read-only logical
//! scans and prints metadata-only row samples. Raw key and value material is
//! never emitted; hashes and byte lengths are enough to correlate against a
//! known synthetic input without turning the dump into a data-exfiltration
//! surface.
//!
//! Usage:
//! `cargo run -p synapse-storage --example dump_cf -- [--include-expired] <db_path> <cf_name>`

use std::{
    error::Error,
    fmt,
    io::{self, Write},
    path::Path,
};

use synapse_storage::{StorageBackendKind, dump_cf_read_only_with_expired};

const USAGE: &str = "usage: dump_cf [--include-expired] <db_path> <cf_name>";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let include_expired = if args.first().is_some_and(|arg| arg == "--include-expired") {
        args.remove(0);
        true
    } else {
        false
    };
    let mut args = args.into_iter();
    let db_path = args.next().ok_or(USAGE)?;
    let cf_name = args.next().ok_or(USAGE)?;
    if let Some(extra) = args.next() {
        return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
    }

    let dump = dump_cf_read_only_with_expired(
        Path::new(&db_path),
        synapse_core::SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        &cf_name,
        include_expired,
    )?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "dump_cf db_path={db_path} cf={} backend={} mode=read_only include_expired={} row_count={}",
            dump.cf_name,
            dump.backend.as_str(),
            include_expired,
            dump.row_count
        ),
    )? {
        return Ok(());
    }
    for (index, row) in dump.rows.iter().enumerate() {
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "row[{index}] key_len_bytes={} key_sha256={} key_material_omitted={} value_len_bytes={} value_sha256={} value_encoding={} value_content_omitted={} redaction_policy={}",
                row.key_len_bytes,
                row.key_sha256,
                row.key_material_omitted,
                row.value_len_bytes,
                row.value_sha256,
                row.value_encoding,
                row.value_content_omitted,
                row.redaction_policy
            ),
        )? {
            return Ok(());
        }
    }
    Ok(())
}

fn write_stdout_line(
    stdout: &mut impl Write,
    args: fmt::Arguments<'_>,
) -> Result<bool, Box<dyn Error>> {
    match stdout
        .write_fmt(args)
        .and_then(|()| stdout.write_all(b"\n"))
    {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(format!(
            "DUMP_CF_STDOUT_WRITE_FAILED kind={:?}: {error}",
            error.kind()
        )
        .into()),
    }
}
