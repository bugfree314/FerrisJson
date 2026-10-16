//! Resident structure budgets derived from total physical RAM, not free RAM or RSS.
pub const DEFAULT_PERCENT: u8 = 25;

pub fn limit_for_total(total: u64, percent: u8) -> Result<usize, String> {
    if !(1..=100).contains(&percent) {
        return Err("Memory budget percentage must be from 1 to 100".into());
    }
    if total == 0 {
        return Err("System total memory is unavailable".into());
    }
    let bytes = u128::from(total) * u128::from(percent) / 100;
    let limit = usize::try_from(bytes)
        .map_err(|_| "Memory budget exceeds this platform's addressable size")?;
    if limit == 0 {
        return Err("Memory budget is too small".into());
    }
    Ok(limit)
}

#[cfg(windows)]
pub fn total_memory() -> Result<u64, String> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // The buffer is initialized and its documented length matches the binding type.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(format!(
            "Cannot read system total memory: {}",
            std::io::Error::last_os_error()
        ));
    }
    if status.ullTotalPhys == 0 {
        return Err("System total memory is unavailable".into());
    }
    Ok(status.ullTotalPhys)
}

#[cfg(not(windows))]
pub fn total_memory() -> Result<u64, String> {
    Err("Total-memory budgets for the resident file backend require Windows".into())
}

pub(super) fn limit(percent: u8) -> Result<usize, String> {
    limit_for_total(total_memory()?, percent)
}
