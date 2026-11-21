//! Serialize capture-failure cleanup with replacement of the active capture.
use std::sync::Mutex;

pub(crate) fn cleanup_current<T>(
    runtime: &Mutex<T>,
    current: impl FnOnce(&T) -> bool,
    cleanup: impl FnOnce(&mut T),
) -> bool {
    let Ok(mut runtime) = runtime.lock() else {
        return false;
    };
    if !current(&runtime) {
        return false;
    }
    cleanup(&mut runtime);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Runtime {
        revision: u64,
        cancelled: bool,
        gaps: u64,
        finish: Option<u64>,
    }
    #[test]
    fn old_capture_failure_does_not_touch_new_meeting_or_its_finish() {
        let state = Mutex::new(Runtime {
            revision: 8,
            finish: Some(42),
            ..Default::default()
        });
        assert!(!cleanup_current(
            &state,
            |r| r.revision == 7 && !r.cancelled,
            |r| {
                r.gaps += 1;
                r.finish = None;
            }
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.gaps, 0);
        assert_eq!(state.finish, Some(42));
    }
    #[test]
    fn cancelled_capture_failure_does_not_run_cleanup_even_if_revision_matches() {
        let state = Mutex::new(Runtime {
            revision: 7,
            cancelled: true,
            finish: Some(42),
            ..Default::default()
        });
        assert!(!cleanup_current(
            &state,
            |r| r.revision == 7 && !r.cancelled,
            |r| {
                r.gaps += 1;
                r.finish = None;
            }
        ));
        assert_eq!(state.lock().unwrap().finish, Some(42));
    }
    #[test]
    fn current_capture_failure_keeps_its_own_cleanup() {
        let state = Mutex::new(Runtime {
            revision: 7,
            finish: Some(41),
            ..Default::default()
        });
        assert!(cleanup_current(
            &state,
            |r| r.revision == 7 && !r.cancelled,
            |r| {
                r.gaps += 1;
                r.finish = None;
            }
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.gaps, 1);
        assert_eq!(state.finish, None);
    }
    #[test]
    fn a_failure_waiting_for_the_lock_rechecks_the_replaced_owner() {
        use std::sync::Arc;
        let state = Arc::new(Mutex::new(Runtime {
            revision: 7,
            finish: Some(42),
            ..Default::default()
        }));
        let mut held = state.lock().unwrap();
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || {
            cleanup_current(
                &worker_state,
                |r| r.revision == 7,
                |r| {
                    r.gaps += 1;
                    r.finish = None;
                },
            )
        });
        held.revision = 8;
        drop(held);
        assert!(!worker.join().unwrap());
        assert_eq!(state.lock().unwrap().finish, Some(42));
    }
}
