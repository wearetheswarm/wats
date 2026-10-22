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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn the_disposition_reads() {
        // Ignored or not depends on how the tests were started (nohup):
        // the call itself must work.
        assert!(super::sighup_ignored().is_some());
    }
}
