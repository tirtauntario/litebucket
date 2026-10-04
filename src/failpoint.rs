//! Test-only crash and fault injection.
//!
//! Compiled to no-ops unless the `failpoints` feature is enabled. When enabled,
//! `STORLITE_FAILPOINTS="name=action,..."` selects actions: `abort` kills the
//! process (SIGABRT, no cleanup), `eio`/`enospc` make an I/O point fail.
//! Failpoints are never reachable remotely.

#[cfg(feature = "failpoints")]
mod imp {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn table() -> &'static Mutex<HashMap<String, String>> {
        static T: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        T.get_or_init(|| {
            let mut m = HashMap::new();
            if let Ok(spec) = std::env::var("STORLITE_FAILPOINTS") {
                for item in spec.split(',').filter(|s| !s.is_empty()) {
                    let (k, v) = item.split_once('=').unwrap_or((item, "abort"));
                    m.insert(k.to_string(), v.to_string());
                }
            }
            Mutex::new(m)
        })
    }

    pub fn action(name: &str) -> Option<String> {
        table().lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned()
    }

    pub fn set(name: &str, action: Option<&str>) {
        let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
        match action {
            Some(a) => t.insert(name.to_string(), a.to_string()),
            None => t.remove(name),
        };
    }
}

/// Crash point: aborts the process when configured.
#[inline]
pub fn hit(_name: &str) {
    #[cfg(feature = "failpoints")]
    if imp::action(_name).as_deref() == Some("abort") {
        eprintln!("failpoint {_name}: abort");
        std::process::abort();
    }
}

/// I/O fault point: returns an injected error or aborts when configured.
#[inline]
pub fn io(_name: &str) -> std::io::Result<()> {
    #[cfg(feature = "failpoints")]
    match imp::action(_name).as_deref() {
        Some("abort") => {
            eprintln!("failpoint {_name}: abort");
            std::process::abort();
        }
        Some("eio") => return Err(rustix::io::Errno::IO.into()),
        Some("enospc") => return Err(rustix::io::Errno::NOSPC.into()),
        _ => {}
    }
    Ok(())
}

/// Configure a failpoint at runtime (feature-gated test helper).
#[cfg(feature = "failpoints")]
pub fn set(name: &str, action: Option<&str>) {
    imp::set(name, action);
}
