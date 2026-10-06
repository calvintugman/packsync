//! Deterministic crash injection for tests.
//!
//! Production code marks every destructive transaction boundary with
//! `crate::failpoint!("restore.after_trash")?;`. Without the `failpoints` cargo feature the macro
//! compiles to nothing. With it, a failpoint can be armed from the environment
//! (`PACKSYNC_FAILPOINTS="restore.after_trash=abort@3,upload.mid=error"`) or programmatically:
//!
//! * `error`  — return [`Error::InjectedCrash`]; every cleanup/rollback path skips work for it, so
//!   the in-process test sees the state a killed process would leave behind;
//! * `abort`  — `std::process::abort()`: real hard termination for subprocess tests;
//! * `panic`  — panic (unwinding; used to test that panics at job boundaries are contained);
//! * `@n`     — fire on the n-th hit only (1-based); without it, on every hit.

use crate::error::Error;

/// Is this error a simulated process death (cleanup code must not "repair" anything)?
pub fn is_crash(e: &Error) -> bool {
    matches!(e, Error::InjectedCrash(_))
}

#[macro_export]
macro_rules! failpoint {
    ($name:expr) => {
        $crate::failpoint::hit($name)
    };
}

#[cfg(not(feature = "failpoints"))]
#[inline(always)]
pub fn hit(_name: &'static str) -> crate::error::Result<()> {
    Ok(())
}

#[cfg(feature = "failpoints")]
mod imp {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Action {
        Error,
        Abort,
        Panic,
    }

    struct Arm {
        action: Action,
        nth: Option<u32>,
        hits: u32,
    }

    fn registry() -> &'static Mutex<HashMap<String, Arm>> {
        static R: OnceLock<Mutex<HashMap<String, Arm>>> = OnceLock::new();
        R.get_or_init(|| {
            let mut m = HashMap::new();
            if let Ok(spec) = std::env::var("PACKSYNC_FAILPOINTS") {
                for part in spec.split(',').filter(|s| !s.is_empty()) {
                    if let Some((name, rest)) = part.split_once('=')
                        && let Some(a) = parse(rest)
                    {
                        m.insert(name.trim().to_string(), a);
                    }
                }
            }
            Mutex::new(m)
        })
    }

    fn parse(s: &str) -> Option<Arm> {
        let (act, nth) = match s.split_once('@') {
            Some((a, n)) => (a, n.parse().ok()),
            None => (s, None),
        };
        let action = match act {
            "error" => Action::Error,
            "abort" => Action::Abort,
            "panic" => Action::Panic,
            _ => return None,
        };
        Some(Arm { action, nth, hits: 0 })
    }

    pub fn arm(name: &str, action: Action, nth: Option<u32>) {
        registry().lock().insert(name.to_string(), Arm { action, nth, hits: 0 });
    }
    pub fn disarm_all() {
        registry().lock().clear();
    }
    pub fn hits(name: &str) -> u32 {
        registry().lock().get(name).map(|a| a.hits).unwrap_or(0)
    }

    pub fn hit(name: &'static str) -> crate::error::Result<()> {
        let action = {
            let mut r = registry().lock();
            let Some(a) = r.get_mut(name) else { return Ok(()) };
            a.hits += 1;
            if a.nth.is_some_and(|n| n != a.hits) {
                return Ok(());
            }
            a.action
        };
        match action {
            Action::Error => Err(Error::InjectedCrash(name.to_string())),
            Action::Abort => std::process::abort(),
            Action::Panic => panic!("injected panic at failpoint '{name}'"),
        }
    }
}

#[cfg(feature = "failpoints")]
pub use imp::{Action, arm, disarm_all, hit, hits};

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    use super::*;

    #[test]
    fn fires_on_the_nth_hit_only() {
        arm("t.point", Action::Error, Some(2));
        assert!(hit("t.point").is_ok());
        let e = hit("t.point").unwrap_err();
        assert!(is_crash(&e));
        assert!(hit("t.point").is_ok());
        assert_eq!(hits("t.point"), 3);
        disarm_all();
        assert!(hit("t.point").is_ok());
    }
}
