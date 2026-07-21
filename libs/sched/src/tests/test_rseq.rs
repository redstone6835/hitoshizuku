//! restartable sequences 判定逻辑测试。

use ktest::ktest;

use crate::rseq::{
    RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE, RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT,
    RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL, RseqCs, RseqError, RseqEvent, RseqEvents, RseqResumeAction,
    decide_resume, validate_signature,
};

const USER_LIMIT: usize = 0x1_0000;

fn cs() -> RseqCs {
    RseqCs {
        version: 0,
        flags: 0,
        start_ip: 0x1000,
        post_commit_offset: 0x20,
        abort_ip: 0x2000,
    }
}

fn events(event: RseqEvent) -> RseqEvents {
    RseqEvents::from_bits(event as u8)
}

#[ktest]
fn pc_inside_critical_section_aborts_for_each_event() {
    for event in [RseqEvent::Preempt, RseqEvent::Signal, RseqEvent::Migrate] {
        assert_eq!(
            decide_resume(0x1010, USER_LIMIT, 0, cs(), events(event)),
            Ok(RseqResumeAction::AbortTo(0x2000))
        );
    }
}

#[ktest]
fn post_commit_boundary_clears_without_abort() {
    assert_eq!(
        decide_resume(0x1020, USER_LIMIT, 0, cs(), events(RseqEvent::Preempt)),
        Ok(RseqResumeAction::Clear)
    );
}

#[ktest]
fn no_restart_flags_mask_matching_events() {
    let cases = [
        (RseqEvent::Preempt, RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT),
        (RseqEvent::Migrate, RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE),
        (
            RseqEvent::Signal,
            RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT
                | RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL
                | RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE,
        ),
    ];
    for (event, flags) in cases {
        let mut critical = cs();
        critical.flags = flags;
        assert_eq!(
            decide_resume(0x1010, USER_LIMIT, 0, critical, events(event)),
            Ok(RseqResumeAction::Keep)
        );
    }
}

#[ktest]
fn signal_mask_requires_preempt_and_migrate_masks() {
    let mut critical = cs();
    critical.flags = RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL;
    assert_eq!(
        decide_resume(0x1010, USER_LIMIT, 0, critical, events(RseqEvent::Signal)),
        Err(RseqError::InvalidFlags)
    );
}

#[ktest]
fn invalid_addresses_and_versions_fail_conservatively() {
    let mut critical = cs();
    critical.version = 1;
    assert_eq!(
        decide_resume(0x1010, USER_LIMIT, 0, critical, events(RseqEvent::Preempt)),
        Err(RseqError::UnsupportedVersion)
    );

    critical = cs();
    critical.start_ip = usize::MAX - 7;
    critical.post_commit_offset = 16;
    assert_eq!(
        decide_resume(
            usize::MAX - 4,
            usize::MAX,
            0,
            critical,
            events(RseqEvent::Preempt)
        ),
        Err(RseqError::AddressOverflow)
    );

    critical = cs();
    critical.abort_ip = 3;
    assert_eq!(
        decide_resume(0x1010, USER_LIMIT, 0, critical, events(RseqEvent::Preempt)),
        Err(RseqError::InvalidAddress)
    );
}

#[ktest]
fn signature_must_match_registration() {
    assert_eq!(validate_signature(0x5305_5305, 0x5305_5305), Ok(()));
    assert_eq!(
        validate_signature(0x5305_5305, 0),
        Err(RseqError::SignatureMismatch)
    );
}
