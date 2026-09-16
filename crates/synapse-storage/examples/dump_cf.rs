//! Offline column-family readback utility.
//!
//! Its output is supporting storage evidence only; manual FSV remains separate.
//!
//! Opens a Synapse `RocksDB` strictly READ-ONLY and prints every row of one
//! column family so physical storage state can be verified independently of
//! the MCP tool surface. Read-only open never creates databases or column
//! families (a source-of-truth reader must not be able to mutate the source
//! of truth) and works even while a live daemon holds the write lock.
//!
//! Usage: `cargo run -p synapse-storage --example dump_cf -- <db_path> <cf_name>`

use std::{
    error::Error,
    fmt,
    io::{self, Write},
};

use rocksdb::{DB, Options};
use synapse_storage::{cf, timeline};

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let db_path = args.next().ok_or("usage: dump_cf <db_path> <cf_name>")?;
    let cf_name = args.next().ok_or("usage: dump_cf <db_path> <cf_name>")?;

    let existing_cfs = DB::list_cf(&Options::default(), &db_path)
        .map_err(|error| format!("not an existing RocksDB at {db_path}: {error}"))?;
    if !existing_cfs.iter().any(|name| name == &cf_name) {
        return Err(format!(
            "column family {cf_name} does not exist in {db_path}; present: {existing_cfs:?}"
        )
        .into());
    }

    let db = DB::open_cf_for_read_only(&Options::default(), &db_path, &existing_cfs, false)?;
    let handle = db
        .cf_handle(&cf_name)
        .ok_or_else(|| format!("column family handle missing after open: {cf_name}"))?;

    let mut row_count = 0_usize;
    let mut rows = Vec::new();
    for item in db.iterator_cf(&handle, rocksdb::IteratorMode::Start) {
        let (key, value) =
            item.map_err(|error| format!("DUMP_CF_ROW_READ_FAILED cf={cf_name}: {error}"))?;
        rows.push((key.to_vec(), value.to_vec()));
        row_count += 1;
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if !write_stdout_line(
        &mut stdout,
        format_args!("dump_cf db_path={db_path} cf={cf_name} mode=read_only row_count={row_count}"),
    )? {
        return Ok(());
    }
    for (index, (key, value)) in rows.iter().enumerate() {
        let key_hex = hex_encode(key);
        let decoded_key = if cf_name == cf::CF_TIMELINE {
            match timeline::decode_timeline_key(key) {
                Ok((ts_ns, seq)) => format!(" ts_ns={ts_ns} seq={seq}"),
                Err(error) => format!(" key_decode_error={error}"),
            }
        } else {
            String::new()
        };
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "row[{index}] key_hex={key_hex}{decoded_key} value={}",
                String::from_utf8_lossy(value)
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
