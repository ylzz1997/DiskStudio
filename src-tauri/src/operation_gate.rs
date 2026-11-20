use std::sync::{
    atomic::{AtomicIsize, Ordering},
    PoisonError, TryLockError,
};

/// Storage operations are exclusive by default. Only surface readers share
/// the gate, and each one owns its permit until its device I/O has drained.
pub(crate) struct OperationGate(AtomicIsize);

const EXCLUSIVE: isize = -1;
const POISONED: isize = -2;

pub(crate) struct OperationGuard<'a> {
    gate: &'a OperationGate,
    exclusive: bool,
}

impl OperationGate {
    pub(crate) const fn new() -> Self {
        Self(AtomicIsize::new(0))
    }

    pub(crate) fn try_lock(&self) -> Result<OperationGuard<'_>, TryLockError<()>> {
        self.0
            .compare_exchange(0, EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .map_err(lock_error)?;
        Ok(OperationGuard {
            gate: self,
            exclusive: true,
        })
    }

    pub(crate) fn try_surface_scan(&self) -> Result<OperationGuard<'_>, TryLockError<()>> {
        let mut count = self.0.load(Ordering::Relaxed);
        loop {
            let next = count
                .checked_add(1)
                .filter(|_| count >= 0)
                .ok_or_else(|| lock_error(count))?;
            match self
                .0
                .compare_exchange_weak(count, next, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
        Ok(OperationGuard {
            gate: self,
            exclusive: false,
        })
    }

    pub(crate) fn surface_scanning(&self) -> bool {
        self.0.load(Ordering::Acquire) > 0
    }
}

fn lock_error(state: isize) -> TryLockError<()> {
    if state == POISONED {
        TryLockError::Poisoned(PoisonError::new(()))
    } else {
        TryLockError::WouldBlock
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        if self.exclusive {
            let next = if std::thread::panicking() {
                POISONED
            } else {
                0
            };
            self.gate.0.store(next, Ordering::Release);
        } else {
            self.gate.0.fetch_sub(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_surface_readers_exclude_other_operations_until_both_release() {
        let gate = OperationGate::new();
        let exclusive = gate.try_lock().unwrap();
        assert!(gate.try_surface_scan().is_err());
        assert!(gate.try_lock().is_err());
        drop(exclusive);

        let first = gate.try_surface_scan().unwrap();
        let second = gate.try_surface_scan().unwrap();
        std::thread::scope(|scope| scope.spawn(move || drop(first)).join().unwrap());
        assert!(gate.surface_scanning());
        assert!(gate.try_lock().is_err());
        drop(second);
        assert!(!gate.surface_scanning());
        assert!(gate.try_lock().is_ok());
    }
}
