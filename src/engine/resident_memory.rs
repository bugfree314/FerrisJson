//! Per-file resident structure budget. This is not a process RSS or free-RAM limit.
pub(super) const SETTING: &str = "FERRISJSON_MEMORY_MIB";
const MIB: u64 = 1024 * 1024;
const DEFAULT_MIB: u64 = 512;
const MIN_MIB: u64 = 64;
const MAX_MIB: u64 = 32_768;

pub(super) fn limit_from_env() -> Result<usize, String> {
    let invalid = || {
        format!(
            "{SETTING} must be a whole number from {MIN_MIB} to {MAX_MIB} MiB. \
             Set it before starting FerrisJson, or remove it to use {DEFAULT_MIB} MiB."
        )
    };
    let mib = match std::env::var(SETTING) {
        Ok(value) => value.trim().parse::<u64>().map_err(|_| invalid())?,
        Err(std::env::VarError::NotPresent) => DEFAULT_MIB,
        Err(std::env::VarError::NotUnicode(_)) => return Err(invalid()),
    };
    if !(MIN_MIB..=MAX_MIB).contains(&mib) {
        return Err(invalid());
    }
    usize::try_from(mib * MIB)
        .map_err(|_| format!("{SETTING} exceeds this platform's addressable budget"))
}
