//! Audio work is fenced by explicit controls, independently of answer progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub revision: u64,
    pub audio_fence: Option<u64>,
    pub command_id: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Current {
    pub revision: Option<u64>,
    pub audio_fence: Option<u64>,
    pub command_id: u64,
    pub enabled: bool,
    pub answer_busy: bool,
}

pub fn skip_reason(stamp: Stamp, current: Current, cancelled: bool) -> Option<&'static str> {
    if cancelled {
        Some("cancelled")
    } else if !current.enabled {
        Some("audio_stopped")
    } else if current.revision != Some(stamp.revision) {
        Some("revision_changed")
    } else if stamp.audio_fence != current.audio_fence {
        Some("control_changed")
    } else if stamp.audio_fence.is_none() && current.command_id != stamp.command_id {
        Some("command_changed")
    } else {
        None
    }
}

pub const QUESTION_QUEUE_CAPACITY: usize = 32;
pub const QUESTION_MAX_AGE_MS: u128 = 600_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    Skip(&'static str),
    Wait,
    Submit(u64),
}

pub fn dispatch(stamp: Stamp, current: Current, cancelled: bool, age_ms: u128) -> Dispatch {
    if let Some(reason) = skip_reason(stamp, current, cancelled) {
        Dispatch::Skip(reason)
    } else if age_ms > QUESTION_MAX_AGE_MS {
        Dispatch::Skip("question_wait_timeout")
    } else if current.answer_busy {
        Dispatch::Wait
    } else {
        Dispatch::Submit(current.command_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn captured() -> Stamp {
        Stamp {
            revision: 7,
            audio_fence: Some(3),
            command_id: 40,
        }
    }
    fn current() -> Current {
        Current {
            revision: Some(7),
            audio_fence: Some(3),
            command_id: 40,
            enabled: true,
            answer_busy: false,
        }
    }
    #[test]
    fn automatic_answer_progress_keeps_queued_and_inflight_audio_valid() {
        let mut live = current();
        // Q1 was accepted while Q2 was captured or being transcribed.
        live.command_id = 41;
        assert_eq!(skip_reason(captured(), live, false), None);
        live.command_id = 42;
        assert_eq!(skip_reason(captured(), live, false), None);
    }
    #[test]
    fn manual_question_or_explicit_cancel_invalidates_old_audio() {
        let mut live = current();
        live.command_id = 41;
        live.audio_fence = Some(4);
        assert_eq!(
            skip_reason(captured(), live, false),
            Some("control_changed")
        );
    }
    #[test]
    fn revision_stop_and_takeover_remain_fenced() {
        let mut live = current();
        live.revision = Some(8);
        assert_eq!(
            skip_reason(captured(), live, false),
            Some("revision_changed")
        );
        live = current();
        live.enabled = false;
        assert_eq!(skip_reason(captured(), live, false), Some("audio_stopped"));
        assert_eq!(skip_reason(captured(), current(), true), Some("cancelled"));
        live = current();
        live.audio_fence = Some(4);
        assert_eq!(
            skip_reason(captured(), live, false),
            Some("control_changed")
        );
    }
    #[test]
    fn legacy_relay_retains_strict_command_fencing() {
        let mut stamp = captured();
        stamp.audio_fence = None;
        let mut live = current();
        live.audio_fence = None;
        live.command_id = 41;
        assert_eq!(skip_reason(stamp, live, false), Some("command_changed"));
    }
    #[test]
    fn next_question_waits_for_previous_answer_then_uses_current_cas() {
        let mut live = current();
        live.command_id = 41;
        live.answer_busy = true;
        assert_eq!(dispatch(captured(), live, false, 5000), Dispatch::Wait);
        live.answer_busy = false;
        assert_eq!(
            dispatch(captured(), live, false, 7000),
            Dispatch::Submit(41)
        );
        live.audio_fence = Some(4);
        assert_eq!(
            dispatch(captured(), live, false, 7000),
            Dispatch::Skip("control_changed")
        );
    }
    #[test]
    fn busy_questions_have_an_explicit_bounded_wait() {
        let mut live = current();
        live.answer_busy = true;
        assert_eq!(
            dispatch(captured(), live, false, QUESTION_MAX_AGE_MS),
            Dispatch::Wait
        );
        assert_eq!(
            dispatch(captured(), live, false, QUESTION_MAX_AGE_MS + 1),
            Dispatch::Skip("question_wait_timeout")
        );
    }
    #[test]
    fn nine_continuous_questions_all_dispatch_in_order_without_cancelling_prior_answers() {
        let pending: std::collections::VecDeque<_> =
            (1..=9).map(|sequence| (sequence, captured())).collect();
        let mut live = current();
        let mut submitted = Vec::new();
        for (sequence, stamp) in pending {
            assert_eq!(
                dispatch(stamp, live, false, sequence * 5000),
                Dispatch::Submit(40 + submitted.len() as u64)
            );
            submitted.push(sequence);
            live.command_id += 1;
            live.answer_busy = true;
            // New captured audio stays queued while this answer is running.
            assert_eq!(
                dispatch(stamp, live, false, sequence * 5000),
                Dispatch::Wait
            );
            live.answer_busy = false;
        }
        assert_eq!(submitted, (1..=9).collect::<Vec<_>>());
    }
    #[test]
    fn manual_control_revokes_every_queued_question_even_after_multiple_auto_answers() {
        let pending = vec![captured(); 9];
        let mut live = current();
        live.command_id += 2;
        live.audio_fence = Some(4);
        for stamp in pending {
            assert_eq!(
                dispatch(stamp, live, false, 4000),
                Dispatch::Skip("control_changed")
            );
        }
    }
}
