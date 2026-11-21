//! The same gate covers validation and every synchronous connection side effect.
use std::sync::Mutex;

pub(crate) fn accepts(
    owner: u64,
    live_owner: u64,
    cancelled: bool,
    incoming: Option<u64>,
    latest: u64,
) -> bool {
    owner == live_owner && !cancelled && incoming.is_none_or(|version| version >= latest)
}
pub(crate) fn checked<R>(
    gate: &Mutex<()>,
    current: impl FnOnce() -> bool,
    action: impl FnOnce() -> R,
) -> Result<Option<R>, &'static str> {
    let _guard = gate.lock().map_err(|_| "连接切换锁异常")?;
    Ok(current().then(action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    };
    #[test]
    fn a_confirmed_pause_rejects_a_late_running_snapshot_and_failure_reset() {
        let gate = Mutex::new(());
        let audio = AtomicBool::new(false);
        for version in [Some(100), Some(99)] {
            let result = checked(
                &gate,
                || accepts(1, 1, false, version, 101),
                || audio.store(true, Ordering::SeqCst),
            )
            .unwrap();
            assert!(result.is_none());
            assert!(!audio.load(Ordering::SeqCst));
        }
        // A failed read carries its pre-read watermark, so it cannot reset the
        // newly acknowledged scope merely because it has no Command payload.
        let reset = AtomicBool::new(false);
        checked(
            &gate,
            || accepts(1, 1, false, Some(100), 101),
            || reset.store(true, Ordering::SeqCst),
        )
        .unwrap();
        assert!(!reset.load(Ordering::SeqCst));
    }
    #[test]
    fn a_pause_ack_waiting_for_the_same_gate_wins_before_the_old_get_applies() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let latest = Arc::new(AtomicU64::new(100));
        let effects = Arc::new(AtomicU64::new(0));
        let (passed, ready) = mpsc::channel();
        let worker_gate = gate.clone();
        let worker_latest = latest.clone();
        let worker_effects = effects.clone();
        let worker = std::thread::spawn(move || {
            // GET returned running v100 before the independent sender's ACK.
            assert!(accepts(1, 1, false, Some(100), 100));
            passed.send(()).unwrap();
            checked(
                &worker_gate,
                || accepts(1, 1, false, Some(100), worker_latest.load(Ordering::SeqCst)),
                || worker_effects.fetch_add(1, Ordering::SeqCst),
            )
            .unwrap()
        });
        ready.recv().unwrap();
        // ACK pause v101 is committed while owning the transition gate.
        latest.store(101, Ordering::SeqCst);
        drop(held);
        assert!(worker.join().unwrap().is_none());
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn replacement_between_cancel_precheck_and_commit_cannot_write_new_state() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let owner = Arc::new(AtomicU64::new(1));
        let cancelled = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicU64::new(0));
        let (passed, ready) = mpsc::channel();
        let worker_gate = gate.clone();
        let worker_owner = owner.clone();
        let worker_cancel = cancelled.clone();
        let worker_writes = writes.clone();
        let worker = std::thread::spawn(move || {
            assert!(!worker_cancel.load(Ordering::SeqCst));
            passed.send(()).unwrap();
            checked(
                &worker_gate,
                || {
                    accepts(
                        1,
                        worker_owner.load(Ordering::SeqCst),
                        worker_cancel.load(Ordering::SeqCst),
                        Some(100),
                        100,
                    )
                },
                || worker_writes.fetch_add(1, Ordering::SeqCst),
            )
            .unwrap()
        });
        ready.recv().unwrap();
        owner.store(2, Ordering::SeqCst);
        cancelled.store(true, Ordering::SeqCst);
        drop(held);
        assert!(worker.join().unwrap().is_none());
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn current_owner_accepts_equal_or_newer_snapshots() {
        assert!(accepts(1, 1, false, Some(101), 101));
        assert!(accepts(1, 1, false, Some(102), 101));
        assert!(accepts(1, 1, false, None, 101));
        assert!(!accepts(1, 2, false, None, 101));
        assert!(!accepts(1, 1, true, None, 101));
    }
}
