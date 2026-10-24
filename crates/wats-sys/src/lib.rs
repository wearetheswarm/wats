//! The few operating-system calls wats makes that need `unsafe`. The
//! `wats` crate is `#![forbid(unsafe_code)]`; what it cannot do safely
//! lives here, one small audited function each, with its safety
//! argument. Nothing here changes the process's state.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

/// Whether SIGHUP is ignored (`SIG_IGN`) in this process, as `nohup`
/// leaves it: read with sigaction(2) and a null new action, which only
/// reads the disposition. `None`: the call failed, or not on Unix.
///
/// Call it before anything registers a SIGHUP handler (tokio replaces an
/// inherited `SIG_IGN` with its own handler), ADR 0031 §3.
pub fn sighup_ignored() -> Option<bool> {
    #[cfg(unix)]
    {
        // SAFETY: `libc::sigaction` is a plain C struct, for which all
        // zero bytes is a valid value. The call passes a null new action,
        // so it changes nothing, and a valid pointer to that struct for
        // the old one, which it fills; nothing else is touched.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) };
        if rc != 0 {
            return None;
        }
        Some(old.sa_sigaction == libc::SIG_IGN)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The local time zone's offset from UTC at `ts` (unix seconds), in
/// seconds east of UTC, read with localtime_r(3). `None`: the call
/// failed, or not on Unix.
pub fn utc_offset(ts: i64) -> Option<i64> {
    #[cfg(unix)]
    {
        // `time_t` is narrower than i64 on some targets.
        #[allow(clippy::useless_conversion)]
        let time: libc::time_t = ts.try_into().ok()?;
        // SAFETY: `libc::tm` is a plain C struct, for which all zero bytes
        // is a valid value. localtime_r reads `time` and fills `tm`, both
        // valid pointers to locals; it is the reentrant form, so no shared
        // buffer is involved.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let filled = unsafe { libc::localtime_r(&time, &mut tm) };
        if filled.is_null() {
            return None;
        }
        Some(tm.tm_gmtoff as i64)
    }
    #[cfg(not(unix))]
    {
        let _ = ts;
        None
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn the_disposition_reads() {
        // Ignored or not depends on how the tests were started (nohup):
        // the call itself must work.
        assert!(super::sighup_ignored().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_utc_offset_reads() {
        let offset = super::utc_offset(1_792_871_756).unwrap();
        assert!(offset.abs() <= 14 * 3600, "{offset}");
    }
}
