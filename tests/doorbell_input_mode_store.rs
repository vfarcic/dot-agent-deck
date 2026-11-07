//! `inbox/doorbell/048`: a generation's input-mode evidence, the store alone
//! (`src/input_mode.rs`).
use dot_agent_deck::input_mode::{
    InputModeStore, MODE_MAX_AGE_NS, ModeClause, ModeSample, Observer, Refusal, Stored,
};
use spec::spec;

const WRAPPER: Observer = Observer {
    pid: 4242,
    start: 1_000,
};

const MS: u64 = 1_000_000;

/// A sample numbered `seq`, read over 1 ms from `at`.
fn sample(seq: u64, icanon: bool, echo: bool, at: u64) -> ModeSample {
    ModeSample {
        seq,
        icanon,
        echo,
        sampled_mono_ns: at,
        sample_done_mono_ns: at + MS,
    }
}

fn raw(seq: u64, at: u64) -> ModeSample {
    sample(seq, false, false, at)
}

/// Scenario: Store one sample at a time with each combination of the two bits, and with none at all. Only a sample with both canonical input and echo off is usable; either bit set reads as cooked, and a generation whose wrapper never reported (an older wrapper, which sends readiness and no probe) reads as unverified.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_only_both_bits_clear_is_usable() {
    let t = 10_000 * MS;
    assert_eq!(
        InputModeStore::default().usable(t),
        Err(ModeClause::Unverified),
        "no report at all"
    );
    for (icanon, echo, want) in [
        (false, false, None),
        (true, false, Some(ModeClause::Cooked)),
        (false, true, Some(ModeClause::Cooked)),
        (true, true, Some(ModeClause::Cooked)),
    ] {
        let mut store = InputModeStore::default();
        assert_eq!(
            store.accept(WRAPPER, sample(1, icanon, echo, t), t + 2 * MS),
            Ok(Stored::Newer)
        );
        assert_eq!(
            store.usable(t + 2 * MS).err(),
            want,
            "icanon {icanon}, echo {echo}"
        );
    }
    assert_eq!(ModeClause::Unverified.as_str(), "input_mode_unverified");
    assert_eq!(ModeClause::Cooked.as_str(), "input_mode_cooked");
    assert_eq!(ModeClause::Stale.as_str(), "input_mode_stale");
}

/// Scenario: Store a raw sample that was taken long before it arrives, then deliver a newer cooked sample late behind an older raw one. A sample is usable only within the age bound counted from the stamp before its read, however recently it arrived and however recent the stamp after the read is; a raw sample stops counting when that age passes, and the newer cooked sample wins the moment it is stored.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_freshness_is_the_samples_age_from_its_first_stamp() {
    let t = 10_000 * MS;
    // Just received, but sampled more than the bound ago.
    let mut store = InputModeStore::default();
    let old = raw(1, t);
    let arrives = t + MODE_MAX_AGE_NS + MS;
    assert_eq!(store.accept(WRAPPER, old, arrives), Ok(Stored::Newer));
    assert_eq!(store.usable(arrives), Err(ModeClause::Stale));

    // Age from the first stamp, not the second: a read that took most of
    // the bound leaves a recent `done` and an old `sampled`.
    let mut store = InputModeStore::default();
    let slow = ModeSample {
        seq: 1,
        icanon: false,
        echo: false,
        sampled_mono_ns: t,
        sample_done_mono_ns: t + MODE_MAX_AGE_NS - MS,
    };
    assert_eq!(
        store.accept(WRAPPER, slow, t + MODE_MAX_AGE_NS),
        Ok(Stored::Newer)
    );
    assert!(store.usable(t + MODE_MAX_AGE_NS).is_ok(), "at the bound");
    assert_eq!(
        store.usable(t + MODE_MAX_AGE_NS + MS),
        Err(ModeClause::Stale),
        "past the bound from `sampled`, though `done` is 2 ms old"
    );

    // A newer cooked report delayed behind an older raw one.
    let mut store = InputModeStore::default();
    assert_eq!(
        store.accept(WRAPPER, raw(1, t), t + 2 * MS),
        Ok(Stored::Newer)
    );
    assert!(store.usable(t + 500 * MS).is_ok());
    assert_eq!(
        store.usable(t + MODE_MAX_AGE_NS + MS),
        Err(ModeClause::Stale),
        "the raw sample alone ages out at its own stamp's bound"
    );
    let cooked = sample(2, true, true, t + 50 * MS);
    assert_eq!(
        store.accept(WRAPPER, cooked, t + 600 * MS),
        Ok(Stored::Newer)
    );
    assert_eq!(store.usable(t + 600 * MS), Err(ModeClause::Cooked));
    assert_eq!(store.latest(), Some(cooked));
}

/// Scenario: Deliver the same sequence number again with fresh stamps, then a lower one, from the bound wrapper. Neither refreshes the evidence: each is counted and ignored, the stored sample stays the one first given that number, and it ages out on its own stamp.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_duplicates_and_regressions_refresh_nothing() {
    let t = 10_000 * MS;
    let mut store = InputModeStore::default();
    let first = raw(5, t);
    assert_eq!(store.accept(WRAPPER, first, t + 2 * MS), Ok(Stored::Newer));
    let later = t + MODE_MAX_AGE_NS;
    assert_eq!(
        store.accept(WRAPPER, raw(5, later - 2 * MS), later),
        Ok(Stored::Duplicate)
    );
    assert_eq!(
        store.accept(WRAPPER, raw(4, later - 2 * MS), later),
        Ok(Stored::Regression)
    );
    assert_eq!(store.latest(), Some(first));
    assert_eq!((store.duplicates, store.regressions), (1, 1));
    assert_eq!(
        store.usable(t + MODE_MAX_AGE_NS + MS),
        Err(ModeClause::Stale),
        "neither report refreshed the evidence"
    );
}

/// Scenario: Offer samples whose second stamp is before the first, whose two stamps are further apart than the age bound, and whose second stamp is later than the daemon's clock. The first two are refused as wide samples and store nothing; the third is a clock mismatch, which holds the generation from then on, so a later sample that is in every way valid is refused too and the evidence reads `clock_namespace`.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_stamps_must_be_ordered_close_and_not_in_the_future() {
    let t = 10_000 * MS;
    let mut store = InputModeStore::default();
    let backwards = ModeSample {
        sample_done_mono_ns: t - 1,
        ..raw(1, t)
    };
    assert_eq!(
        store.accept(WRAPPER, backwards, t + MS),
        Err(Refusal::WideSample)
    );
    let wide = ModeSample {
        sample_done_mono_ns: t + MODE_MAX_AGE_NS + 1,
        ..raw(2, t)
    };
    assert_eq!(
        store.accept(WRAPPER, wide, t + 2 * MODE_MAX_AGE_NS),
        Err(Refusal::WideSample)
    );
    assert_eq!(Refusal::WideSample.code(), "wide_sample");
    assert_eq!(store.latest(), None);
    assert_eq!(store.observer(), None, "a refused sample binds nobody");
    assert_eq!(store.refused, 2);
    assert_eq!(store.usable(t + 3 * MS), Err(ModeClause::Unverified));

    let future = raw(3, t + 10 * MS);
    assert_eq!(
        store.accept(WRAPPER, future, t + 5 * MS),
        Err(Refusal::ClockMismatch)
    );
    assert_eq!(store.sticky(), Some(ModeClause::ClockNamespace));
    assert_eq!(
        store.accept(WRAPPER, raw(4, t + 20 * MS), t + 30 * MS),
        Err(Refusal::Held(ModeClause::ClockNamespace))
    );
    assert_eq!(store.usable(t + 30 * MS), Err(ModeClause::ClockNamespace));
    assert_eq!(ModeClause::ClockNamespace.as_str(), "clock_namespace");
}

/// Scenario: Store a cooked sample, then a raw one, then another raw one. The settle anchor is taken once, from the stamp after the read of the first raw sample: a cooked sample does not set it, and a later raw sample does not move it.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_the_settle_anchor_is_the_first_raw_samples_second_stamp() {
    let t = 10_000 * MS;
    let mut store = InputModeStore::default();
    store
        .accept(WRAPPER, sample(1, true, true, t), t + 2 * MS)
        .unwrap();
    assert_eq!(store.anchor(), None, "a cooked sample is no anchor");
    let first = ModeSample {
        sample_done_mono_ns: t + 57 * MS,
        ..raw(2, t + 50 * MS)
    };
    store.accept(WRAPPER, first, t + 60 * MS).unwrap();
    assert_eq!(
        store.anchor(),
        Some(t + 57 * MS),
        "from `done`, not `sampled`"
    );
    store
        .accept(WRAPPER, raw(3, t + 100 * MS), t + 110 * MS)
        .unwrap();
    assert_eq!(store.anchor(), Some(t + 57 * MS), "set once");
}

/// Scenario: A second process reports for a generation another process already bound, and then the bound one reports again. The second report is refused as a conflict naming both processes, and the generation holds `input_mode_conflict` from then on, so even the bound process's next report is refused and nothing it sent counts.
#[spec("inbox/doorbell/048")]
#[test]
fn doorbell_048_a_second_observer_holds_the_generation() {
    let t = 10_000 * MS;
    let other = Observer {
        pid: 4243,
        start: 1_001,
    };
    let mut store = InputModeStore::default();
    store.accept(WRAPPER, raw(1, t), t + 2 * MS).unwrap();
    assert_eq!(
        store.accept(other, raw(1, t + 10 * MS), t + 12 * MS),
        Err(Refusal::Conflict)
    );
    assert_eq!(store.conflict(), Some((WRAPPER, other)));
    assert_eq!(
        store.accept(WRAPPER, raw(2, t + 20 * MS), t + 22 * MS),
        Err(Refusal::Held(ModeClause::Conflict))
    );
    assert_eq!(store.usable(t + 22 * MS), Err(ModeClause::Conflict));
    assert_eq!(ModeClause::Conflict.as_str(), "input_mode_conflict");
    assert!(ModeClause::Conflict.sticky());
    assert!(!ModeClause::Stale.sticky() && !ModeClause::Cooked.sticky());
}
