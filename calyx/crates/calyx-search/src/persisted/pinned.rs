use super::stale;
use crate::error::CliResult;
use std::path::Path;

pub(crate) fn canonical_vault_dir(vault_dir: &Path) -> CliResult<String> {
    let canonical = std::fs::canonicalize(vault_dir).map_err(|err| {
        stale(format!(
            "search cache cannot canonicalize vault path {}: {err}",
            vault_dir.display()
        ))
    })?;
    Ok(canonical.display().to_string())
}
