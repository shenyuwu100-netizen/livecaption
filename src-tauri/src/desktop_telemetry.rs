//! One coalescing telemetry sender. Slow reporting never owns the command socket.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[path = "connection_commit.rs"]
mod connection_commit;
pub(crate) use connection_commit::accepts as accepts_command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Meeting,
}

pub struct Sample<T> {
    pub key: String,
    pub body: T,
    pub active: bool,
}

#[derive(Debug)]
pub enum Delivery {
    Accepted,
    Retry,
    Revoked,
    Quota(Duration),
}

#[derive(Clone, Copy)]
pub struct Schedule {
    pub tick: Duration,
    pub active_heartbeat: Duration,
    pub idle_heartbeat: Duration,
}
impl Default for Schedule {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(500),
            active_heartbeat: Duration::from_secs(2),
            idle_heartbeat: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Default)]
pub struct ConnectionTransitions {
    gate: Arc<Mutex<()>>,
    generation: Arc<AtomicU64>,
}
impl ConnectionTransitions {
    pub fn begin(&self) -> Result<u64, &'static str> {
        self.commit(|| self.generation.fetch_add(1, Ordering::SeqCst) + 1)
    }
    pub fn commit_current<R>(
        &self,
        generation: u64,
        action: impl FnOnce() -> R,
    ) -> Result<Option<R>, &'static str> {
        self.commit(|| (self.generation.load(Ordering::SeqCst) == generation).then(action))
    }
    pub fn disconnect<R>(&self, action: impl FnOnce() -> R) -> Result<R, &'static str> {
        self.commit(|| {
            self.generation.fetch_add(1, Ordering::SeqCst);
            action()
        })
    }
    pub fn commit<R>(&self, action: impl FnOnce() -> R) -> Result<R, &'static str> {
        let _transition = self.gate.lock().map_err(|_| "连接切换锁异常")?;
        Ok(action())
    }
    pub fn commit_checked<R>(
        &self,
        current: impl FnOnce() -> bool,
        action: impl FnOnce() -> R,
    ) -> Result<Option<R>, &'static str> {
        connection_commit::checked(&self.gate, current, action)
    }
    pub fn revoke<R>(
        &self,
        cancel: &CancellationToken,
        cleanup: impl FnOnce() -> R,
    ) -> Result<Option<R>, &'static str> {
        let _transition = self.gate.lock().map_err(|_| "连接切换锁异常")?;
        // A replacement may have cancelled this token while we waited for the
        // gate. Ownership must be checked here, before touching global state.
        if cancel.is_cancelled() {
            return Ok(None);
        }
        cancel.cancel();
        Ok(Some(cleanup()))
    }
}

pub fn request_timeout(kind: Kind) -> Duration {
    Duration::from_secs(match kind {
        Kind::Audio => 2,
        // Leave room for the next complete audio request inside the server's
        // six-second lease even after a slow audio ACK and a scheduler tick.
        Kind::Meeting => 1,
    })
}

pub async fn run<C, T, Collect, Send, Sending, Backoff>(
    controls: watch::Receiver<Option<C>>,
    cancel: CancellationToken,
    schedule: Schedule,
    collect: Collect,
    send: Send,
    backoff: Backoff,
) where
    C: Clone + PartialEq,
    Collect: FnMut(&C, Kind) -> Option<Sample<T>>,
    Send: FnMut(C, Kind, T) -> Sending,
    Sending: Future<Output = Delivery>,
    Backoff: FnMut() -> Duration,
{
    let origin = tokio::time::Instant::now();
    run_with_clock(
        controls,
        cancel,
        schedule,
        collect,
        send,
        backoff,
        move || origin.elapsed(),
    )
    .await;
}

async fn run_with_clock<C, T, Collect, Send, Sending, Backoff, Clock>(
    mut controls: watch::Receiver<Option<C>>,
    cancel: CancellationToken,
    schedule: Schedule,
    mut collect: Collect,
    mut send: Send,
    mut backoff: Backoff,
    mut now: Clock,
) where
    C: Clone + PartialEq,
    Collect: FnMut(&C, Kind) -> Option<Sample<T>>,
    Send: FnMut(C, Kind, T) -> Sending,
    Sending: Future<Output = Delivery>,
    Backoff: FnMut() -> Duration,
    Clock: FnMut() -> Duration,
{
    let mut ticks = tokio::time::interval(schedule.tick);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_audio: Option<(String, Duration)> = None;
    let mut last_meeting: Option<String> = None;
    let mut retry_at = now();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            changed = controls.changed() => if changed.is_err() { break; },
            _ = ticks.tick() => {},
        }
        if now() < retry_at || !backoff().is_zero() {
            continue;
        }
        let Some(control) = controls.borrow_and_update().clone() else {
            continue;
        };
        for kind in [Kind::Audio, Kind::Meeting] {
            // A slow audio send can span a pause or takeover. Never build the
            // following meeting report from the superseded control snapshot.
            if controls.borrow().as_ref() != Some(&control)
                || cancel.is_cancelled()
                || !backoff().is_zero()
            {
                break;
            }
            let Some(sample) = collect(&control, kind) else {
                continue;
            };
            let due = match kind {
                Kind::Audio => last_audio.as_ref().is_none_or(|(key, sent)| {
                    key != &sample.key
                        || now().saturating_sub(*sent)
                            >= if sample.active {
                                schedule.active_heartbeat
                            } else {
                                schedule.idle_heartbeat
                            }
                }),
                Kind::Meeting => last_meeting.as_ref() != Some(&sample.key),
            };
            if !due {
                continue;
            }
            let attempted_at = now();
            let delivery = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                delivery = send(control.clone(), kind, sample.body) => delivery,
            };
            match delivery {
                Delivery::Accepted => match kind {
                    Kind::Audio => last_audio = Some((sample.key, attempted_at)),
                    Kind::Meeting => last_meeting = Some(sample.key),
                },
                Delivery::Retry => {}
                Delivery::Revoked => {
                    cancel.cancel();
                    return;
                }
                Delivery::Quota(delay) => {
                    retry_at = now() + delay;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, Semaphore};

    fn schedule() -> Schedule {
        Schedule {
            tick: Duration::from_millis(2),
            active_heartbeat: Duration::from_millis(15),
            idle_heartbeat: Duration::from_millis(30),
        }
    }
    async fn next(rx: &mut mpsc::UnboundedReceiver<(Kind, u64)>) -> (Kind, u64) {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("independent telemetry sender did not make progress")
            .expect("telemetry sender stopped")
    }
    fn sample(revision: &u64, kind: Kind) -> Option<Sample<u64>> {
        Some(Sample {
            key: format!("{revision}-{kind:?}"),
            body: *revision,
            active: true,
        })
    }

    #[test]
    fn reconnect_waits_for_all_old_cleanup_before_committing_new_state() {
        let transitions = ConnectionTransitions::default();
        let old = CancellationToken::new();
        let global_state = Arc::new(Mutex::new(1u64));
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let retire = transitions.clone();
        let cleanup_state = global_state.clone();
        let cleanup = std::thread::spawn(move || {
            retire
                .revoke(&old, || {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    *cleanup_state.lock().unwrap() = 0;
                })
                .unwrap();
        });
        ready.recv().unwrap();
        let (committed, done) = std::sync::mpsc::channel();
        let install_state = global_state.clone();
        let reconnect = std::thread::spawn(move || {
            transitions
                .commit(|| {
                    *install_state.lock().unwrap() = 2;
                    committed.send(()).unwrap();
                })
                .unwrap();
        });
        let serialized = done.recv_timeout(Duration::from_millis(40)).is_err();
        release.send(()).unwrap();
        cleanup.join().unwrap();
        reconnect.join().unwrap();
        assert!(
            serialized,
            "new session committed while old global cleanup was still running"
        );
        assert_eq!(
            *global_state.lock().unwrap(),
            2,
            "old cleanup erased the new session"
        );
    }

    #[test]
    fn stale_revocation_rechecks_ownership_after_waiting_for_connection_commit() {
        let transitions = ConnectionTransitions::default();
        let old = CancellationToken::new();
        let old_for_install = old.clone();
        let global_state = Arc::new(Mutex::new(1u64));
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let install_state = global_state.clone();
        let install = transitions.clone();
        let commit = std::thread::spawn(move || {
            install
                .commit(|| {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    old_for_install.cancel();
                    *install_state.lock().unwrap() = 2;
                })
                .unwrap();
        });
        ready.recv().unwrap();
        let (retiring, retire_started) = std::sync::mpsc::channel();
        let cleanup_state = global_state.clone();
        let retire = std::thread::spawn(move || {
            retiring.send(()).unwrap();
            transitions
                .revoke(&old, || *cleanup_state.lock().unwrap() = 0)
                .unwrap()
        });
        retire_started.recv().unwrap();
        // The new install still owns the transition; let the old revoke reach it.
        std::thread::sleep(Duration::from_millis(40));
        release.send(()).unwrap();
        commit.join().unwrap();
        assert!(
            retire.join().unwrap().is_none(),
            "superseded token still ran global cleanup"
        );
        assert_eq!(*global_state.lock().unwrap(), 2);
    }

    #[test]
    fn manual_disconnect_fences_a_connection_still_authenticating() {
        let transitions = ConnectionTransitions::default();
        let pending = transitions.begin().unwrap();
        let global_state = std::cell::Cell::new(1);
        transitions.disconnect(|| global_state.set(0)).unwrap();
        let installed = transitions
            .commit_current(pending, || global_state.set(2))
            .unwrap();
        assert!(
            installed.is_none(),
            "a pending connect committed after manual disconnect"
        );
        assert_eq!(global_state.get(), 0);
    }

    #[tokio::test]
    async fn virtual_clock_keeps_slow_reports_inside_desktop_six_second_lease() {
        use std::sync::atomic::AtomicU64;
        let (_controls, receiver) = watch::channel(Some(1u64));
        let cancel = CancellationToken::new();
        let clock = Arc::new(AtomicU64::new(0));
        let send_clock = clock.clone();
        let read_clock = clock.clone();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_with_clock(
            receiver,
            cancel.clone(),
            Schedule {
                tick: Duration::from_millis(2),
                ..Schedule::default()
            },
            sample,
            move |_, kind, _| {
                let started = send_clock.load(Ordering::SeqCst);
                events.send((kind, started)).unwrap();
                // Simulate the full configured request budget, without sleeping
                // real seconds or enabling Tokio's test-only clock in the app.
                send_clock.fetch_add(request_timeout(kind).as_millis() as u64, Ordering::SeqCst);
                std::future::ready(Delivery::Accepted)
            },
            || Duration::ZERO,
            move || Duration::from_millis(read_clock.load(Ordering::SeqCst)),
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 0));
        assert_eq!(next(&mut observed).await.0, Kind::Meeting);
        let (kind, next_start) = next(&mut observed).await;
        assert_eq!(kind, Kind::Audio);
        // Server TTL is 6000ms. Reserve a real 500ms production scheduler tick
        // and the next complete 2000ms audio request, with another 500ms margin.
        assert!(
            next_start + 2000 + 500 <= 5500,
            "heartbeat can expire before next request arrives: {next_start}"
        );
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn slow_report_coalesces_controls_and_never_sends_an_old_meeting_revision() {
        let (controls, receiver) = watch::channel(Some(1u64));
        let cancel = CancellationToken::new();
        let gate = Arc::new(Semaphore::new(0));
        let (events, mut observed) = mpsc::unbounded_channel();
        let worker_gate = gate.clone();
        let worker_cancel = cancel.clone();
        let task = tokio::spawn(run(
            receiver,
            worker_cancel,
            schedule(),
            sample,
            move |revision, kind, body| {
                let events = events.clone();
                let gate = worker_gate.clone();
                async move {
                    events.send((kind, body)).unwrap();
                    if revision == 1 && kind == Kind::Audio {
                        gate.acquire().await.unwrap().forget();
                    }
                    Delivery::Accepted
                }
            },
            || Duration::ZERO,
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 1));
        // Controls remain writable while network reporting is stalled. Only the
        // latest control is retained; intermediate revisions must never be sent.
        for revision in 2..=5000 {
            controls.send_replace(Some(revision));
        }
        gate.add_permits(1);
        assert_eq!(next(&mut observed).await, (Kind::Audio, 5000));
        assert_eq!(next(&mut observed).await, (Kind::Meeting, 5000));
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn idle_desktop_still_renews_its_heartbeat_without_command_updates() {
        let (_controls, receiver) = watch::channel(Some(7u64));
        let cancel = CancellationToken::new();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            receiver,
            cancel.clone(),
            schedule(),
            |revision, kind| {
                (kind == Kind::Audio).then(|| Sample {
                    key: "idle-7".into(),
                    body: *revision,
                    active: false,
                })
            },
            move |_, kind, body| {
                events.send((kind, body)).unwrap();
                std::future::ready(Delivery::Accepted)
            },
            || Duration::ZERO,
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 7));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 7));
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn quota_backoff_blocks_both_reports_then_sends_only_latest_control() {
        let (controls, receiver) = watch::channel(Some(1u64));
        let cancel = CancellationToken::new();
        let attempted = Arc::new(AtomicUsize::new(0));
        let worker_attempted = attempted.clone();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            receiver,
            cancel.clone(),
            schedule(),
            sample,
            move |_, kind, body| {
                events.send((kind, body)).unwrap();
                let result = if worker_attempted.fetch_add(1, Ordering::SeqCst) == 0 {
                    Delivery::Quota(Duration::from_millis(100))
                } else {
                    Delivery::Accepted
                };
                std::future::ready(result)
            },
            || Duration::ZERO,
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 1));
        controls.send_replace(Some(2));
        assert!(
            tokio::time::timeout(Duration::from_millis(35), observed.recv())
                .await
                .is_err()
        );
        assert_eq!(attempted.load(Ordering::SeqCst), 1);
        assert_eq!(next(&mut observed).await, (Kind::Audio, 2));
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn command_loop_quota_backoff_also_pauses_the_background_sender() {
        let (_controls, receiver) = watch::channel(Some(3u64));
        let cancel = CancellationToken::new();
        let paused = Arc::new(AtomicBool::new(true));
        let worker_paused = paused.clone();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            receiver,
            cancel.clone(),
            schedule(),
            sample,
            move |_, kind, body| {
                events.send((kind, body)).unwrap();
                std::future::ready(Delivery::Accepted)
            },
            move || {
                if worker_paused.load(Ordering::SeqCst) {
                    Duration::from_secs(1)
                } else {
                    Duration::ZERO
                }
            },
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(35), observed.recv())
                .await
                .is_err()
        );
        paused.store(false, Ordering::SeqCst);
        assert_eq!(next(&mut observed).await, (Kind::Audio, 3));
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_inflight_report_and_revocation_stops_the_pump() {
        let (_controls, receiver) = watch::channel(Some(1u64));
        let cancel = CancellationToken::new();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            receiver,
            cancel.clone(),
            schedule(),
            sample,
            move |_, kind, body| {
                events.send((kind, body)).unwrap();
                std::future::pending::<Delivery>()
            },
            || Duration::ZERO,
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 1));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        let (_controls, receiver) = watch::channel(Some(2u64));
        let revoked = CancellationToken::new();
        let sends = Arc::new(AtomicUsize::new(0));
        let worker_sends = sends.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            run(
                receiver,
                revoked.clone(),
                schedule(),
                sample,
                move |_, _, _| {
                    worker_sends.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Delivery::Revoked)
                },
                || Duration::ZERO,
            ),
        )
        .await
        .unwrap();
        assert!(revoked.is_cancelled());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_meeting_retries_latest_snapshot_without_replaying_acknowledged_meetings() {
        let (_controls, receiver) = watch::channel(Some(4u64));
        let cancel = CancellationToken::new();
        let changed = Arc::new(AtomicBool::new(false));
        let collect_changed = changed.clone();
        let worker_changed = changed.clone();
        let (events, mut observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            receiver,
            cancel.clone(),
            schedule(),
            move |_, kind| {
                let version = if collect_changed.load(Ordering::SeqCst) {
                    5
                } else {
                    4
                };
                Some(Sample {
                    key: format!("{kind:?}-{version}"),
                    body: version,
                    active: true,
                })
            },
            move |_, kind, body| {
                events.send((kind, body)).unwrap();
                let result = if kind == Kind::Meeting && body == 4 {
                    worker_changed.store(true, Ordering::SeqCst);
                    Delivery::Retry
                } else {
                    Delivery::Accepted
                };
                std::future::ready(result)
            },
            || Duration::ZERO,
        ));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 4));
        assert_eq!(next(&mut observed).await, (Kind::Meeting, 4));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 5));
        assert_eq!(next(&mut observed).await, (Kind::Meeting, 5));
        assert_eq!(next(&mut observed).await, (Kind::Audio, 5));
        cancel.cancel();
        task.await.unwrap();
    }
}
