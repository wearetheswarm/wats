//! The few operating-system calls wats makes that need `unsafe`. The
//! `wats` crate is `#![forbid(unsafe_code)]`; what it cannot do safely
//! lives here, one small audited function each, with its safety
//! argument. Nothing here changes the process's state, except
//! [`background_priority`], which lowers the calling thread's priority.
//!
//! [`sgemm`] is the matrix product of semantic search (ADR 0053): Apple's
//! Accelerate on macOS, the pure-Rust `gemm` crate elsewhere, both behind
//! one bounds-checked safe function.

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

/// `c = a × b`, in f32, row-major: `a` is m×k (row stride `lda`); `b` is
/// k×n (row stride `ldb`), or, when `bt`, given transposed as n×k rows
/// (row stride `ldb`); `c` is m×n (row stride `ldc`), overwritten. One
/// thread. Panics when a slice is too short for its dimensions and stride:
/// a caller's bug, never undefined behaviour.
#[allow(clippy::too_many_arguments)]
pub fn sgemm(
    m: usize,
    n: usize,
    k: usize,
    a: &[f32],
    lda: usize,
    b: &[f32],
    ldb: usize,
    bt: bool,
    c: &mut [f32],
    ldc: usize,
) {
    if m == 0 || n == 0 {
        return;
    }
    if k == 0 {
        for row in c.chunks_mut(ldc).take(m) {
            row.iter_mut().take(n).for_each(|v| *v = 0.0);
        }
        return;
    }
    // The last element each operand is read or written at, so every
    // access below is in bounds.
    let fits = |rows: usize, cols: usize, stride: usize, len: usize| {
        stride >= cols
            && (rows - 1)
                .checked_mul(stride)
                .and_then(|x| x.checked_add(cols))
                .is_some_and(|end| end <= len)
    };
    assert!(fits(m, k, lda, a.len()), "sgemm: a is too short");
    if bt {
        assert!(fits(n, k, ldb, b.len()), "sgemm: b is too short");
    } else {
        assert!(fits(k, n, ldb, b.len()), "sgemm: b is too short");
    }
    assert!(fits(m, n, ldc, c.len()), "sgemm: c is too short");
    #[cfg(target_os = "macos")]
    {
        let int = |v: usize| i32::try_from(v).expect("sgemm: dimension over i32");
        // SAFETY: the asserts above check that `a`, `b` and `c` cover
        // every element cblas_sgemm reads or writes for these dimensions
        // and leading dimensions (row-major: element (i, j) of a matrix
        // with leading dimension ld is at i * ld + j). `c` is a unique
        // borrow, so it aliases neither input. beta = 0: `c` is only
        // written.
        unsafe {
            cblas_sgemm(
                CBLAS_ROW_MAJOR,
                CBLAS_NO_TRANS,
                if bt { CBLAS_TRANS } else { CBLAS_NO_TRANS },
                int(m),
                int(n),
                int(k),
                1.0,
                a.as_ptr(),
                int(lda),
                b.as_ptr(),
                int(ldb),
                0.0,
                c.as_mut_ptr(),
                int(ldc),
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let stride = |v: usize| isize::try_from(v).expect("sgemm: stride over isize");
        // b's column and row strides: element (kk, j) of the k×n operand.
        let (rhs_cs, rhs_rs) = if bt {
            (stride(ldb), 1)
        } else {
            (1, stride(ldb))
        };
        // SAFETY: as above, the asserts check that every element gemm
        // reads (a, b) or writes (c) is inside its slice, with these
        // strides: element (i, j) of `a` at i * lda + j, of `c` at
        // i * ldc + j, of `b` at kk * rhs_rs + j * rhs_cs. `c` is a unique
        // borrow. read_dst = false: `c` is only written.
        unsafe {
            gemm::gemm(
                m,
                n,
                k,
                c.as_mut_ptr(),
                1,
                stride(ldc),
                false,
                a.as_ptr(),
                1,
                stride(lda),
                b.as_ptr(),
                rhs_cs,
                rhs_rs,
                0.0,
                1.0,
                false,
                false,
                false,
                gemm::Parallelism::None,
            );
        }
    }
}

#[cfg(target_os = "macos")]
const CBLAS_ROW_MAJOR: i32 = 101;
#[cfg(target_os = "macos")]
const CBLAS_NO_TRANS: i32 = 111;
#[cfg(target_os = "macos")]
const CBLAS_TRANS: i32 = 112;

#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn cblas_sgemm(
        order: i32,
        trans_a: i32,
        trans_b: i32,
        m: i32,
        n: i32,
        k: i32,
        alpha: f32,
        a: *const f32,
        lda: i32,
        b: *const f32,
        ldb: i32,
        beta: f32,
        c: *mut f32,
        ldc: i32,
    );
}

/// Lowers the calling thread's priority for background work (semantic
/// search's backfill, ADR 0053 §6): the background QoS class on macOS,
/// which also keeps it on the efficiency cores, and a nice value of 19 on
/// Linux. Only this thread changes. Returns whether it worked.
pub fn background_priority() -> bool {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: pthread_set_qos_class_self_np only changes the QoS class
        // of the calling thread; it takes two plain values.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0) == 0
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid takes nothing; setpriority with PRIO_PROCESS and
        // a thread id changes that thread's nice value only (Linux
        // threads are scheduled entities of their own).
        unsafe {
            let tid = libc::gettid();
            #[allow(clippy::useless_conversion)]
            let who: libc::id_t = match tid.try_into() {
                Ok(who) => who,
                Err(_) => return false,
            };
            libc::setpriority(libc::PRIO_PROCESS, who, 19) == 0
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// The 1-minute load average, read with getloadavg(3). `None`: the call
/// failed, or not on Unix.
pub fn load_average() -> Option<f64> {
    #[cfg(unix)]
    {
        let mut load = [0f64; 1];
        // SAFETY: getloadavg writes at most `nelem` (1) doubles into the
        // buffer, which holds 1.
        let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
        (n == 1).then_some(load[0])
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    /// 2×3 times 3×2, plainly and transposed, with strides.
    #[test]
    fn sgemm_multiplies() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let b = [7.0, 8.0, 9.0, 10.0, 11.0, 12.0];
        let mut c = [0.0; 4];
        super::sgemm(2, 2, 3, &a, 3, &b, 2, false, &mut c, 2);
        assert_eq!(c, [58.0, 64.0, 139.0, 154.0]);
        // b given transposed: rows (7, 9, 11) and (8, 10, 12).
        let bt = [7.0, 9.0, 11.0, 8.0, 10.0, 12.0];
        let mut c = [0.0; 4];
        super::sgemm(2, 2, 3, &a, 3, &bt, 3, true, &mut c, 2);
        assert_eq!(c, [58.0, 64.0, 139.0, 154.0]);
        // Into a wider matrix: column 1 of a 2×3 row-major c.
        let mut c = [0.0; 6];
        super::sgemm(2, 1, 3, &a, 3, &bt, 3, true, &mut c[1..], 3);
        assert_eq!(c, [0.0, 58.0, 0.0, 0.0, 139.0, 0.0]);
    }

    /// Against a naive product, over many shapes (those of BERT's layers,
    /// odd ones, a single row or column) and strides, both ways round:
    /// whichever kernels the CPU gets (AVX-512, AVX2, NEON, Accelerate).
    #[test]
    fn sgemm_matches_a_naive_product() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let dims = [1usize, 2, 3, 7, 16, 17, 32, 33, 64, 130];
        let mut checked = 0;
        for &m in &dims {
            for &n in &[1usize, 5, 16, 31, 64, 384] {
                for &k in &[1usize, 8, 15, 32, 384] {
                    for bt in [false, true] {
                        let (lda, ldc) = (k + m % 3, n + 2);
                        let ldb = if bt { k + 1 } else { n + 3 };
                        let a: Vec<f32> = (0..(m - 1) * lda + k).map(|_| next()).collect();
                        let b_len = if bt {
                            (n - 1) * ldb + k
                        } else {
                            (k - 1) * ldb + n
                        };
                        let b: Vec<f32> = (0..b_len).map(|_| next()).collect();
                        let mut c = vec![f32::NAN; (m - 1) * ldc + n];
                        super::sgemm(m, n, k, &a, lda, &b, ldb, bt, &mut c, ldc);
                        for i in 0..m {
                            for j in 0..n {
                                let want: f64 = (0..k)
                                    .map(|p| {
                                        let bv = if bt { b[j * ldb + p] } else { b[p * ldb + j] };
                                        f64::from(a[i * lda + p]) * f64::from(bv)
                                    })
                                    .sum();
                                let got = f64::from(c[i * ldc + j]);
                                assert!(
                                    (got - want).abs() <= 1e-4 * (1.0 + want.abs()),
                                    "m={m} n={n} k={k} bt={bt} ({i},{j}): {got} vs {want}"
                                );
                            }
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, dims.len() * 6 * 5 * 2);
    }

    #[test]
    #[should_panic(expected = "a is too short")]
    fn sgemm_refuses_short_slices() {
        let mut c = [0.0; 4];
        super::sgemm(2, 2, 3, &[1.0; 5], 3, &[1.0; 6], 2, false, &mut c, 2);
    }

    #[cfg(unix)]
    #[test]
    fn the_load_average_reads() {
        assert!(super::load_average().is_some_and(|l| l >= 0.0));
    }

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
