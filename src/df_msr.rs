use std::sync::{Mutex, MutexGuard};

static DF_MSR_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock() -> MutexGuard<'static, ()> {
    DF_MSR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
