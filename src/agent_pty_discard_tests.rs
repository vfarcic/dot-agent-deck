//! PRD #1666 part 3 (Q35-20e review, R-E-U2-01 and R-E-U3-02): the
//! registry's not-delivered notice, over real spawned generations sharing a
//! pane: which retirement writes it, what leaves it standing, and how long
//! it lives (`inbox/probe/082`).

use super::*;
use crate::composer_probe::gate::Admission;
use spec::spec;

fn opts<'a>(pane: &str, command: &'a str) -> SpawnOptions<'a> {
    SpawnOptions {
        command: Some(command),
        env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
        ..SpawnOptions::default()
    }
}

/// Spawn `/usr/bin/true` on `pane` and wait until it has exited, so the
/// next spawn can take the pane while this record lingers.
async fn spawn_exited(registry: &Arc<AgentPtyRegistry>, pane: &str) -> String {
    let id = registry
        .spawn_agent(opts(pane, "/usr/bin/true"))
        .expect("spawn a generation");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while registry.live_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "generation {id} never exited"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    id
}

/// Have `id`'s gate hold `bytes` of typing, as a fence holds a person's
/// keys: fenced, admitted and counted in flight, so its retirement refuses
/// and counts it.
fn hold_typing(registry: &AgentPtyRegistry, id: &str, bytes: &[u8]) {
    hold(registry, id, UserFrame::new(bytes.to_vec(), None, None));
}

/// Have `id`'s gate hold a voice answer's keys, as `write_answer` offers
/// them: the frame carries its answer gate.
fn hold_answer(registry: &AgentPtyRegistry, id: &str, bytes: &[u8]) {
    let (reply, _done) = oneshot::channel();
    let mut frame = UserFrame::new(bytes.to_vec(), None, None);
    frame.answer = Some(HeldAnswer {
        gate: AnswerGate {
            raised: Arc::new(Mutex::new(HashMap::new())),
            pane_id: "p".into(),
            question_id: "q".into(),
            revision: None,
            reservation: None,
            pause: None,
        },
        reply,
    });
    hold(registry, id, frame);
}

fn hold(registry: &AgentPtyRegistry, id: &str, frame: UserFrame) {
    let gate = Arc::clone(&registry.inner.lock().agents[id].gate);
    assert!(gate.gate.fence(0), "{id}'s gate is not retired yet");
    let len = frame.bytes.len();
    let admitted = gate.gate.admit(
        frame,
        len,
        None,
        |_, _| false,
        |_| {
            gate.in_flight.count.fetch_add(1, Ordering::SeqCst);
        },
    );
    assert!(matches!(admitted, Admission::Held), "{id} holds the frame");
}

/// Whether `first` comes before `second` in the registry's own iteration
/// order, the order a publish visits a pane's records in.
fn visits_first(registry: &AgentPtyRegistry, first: &str, second: &str) -> bool {
    let inner = registry.inner.lock();
    let order: Vec<&String> = inner.agents.keys().collect();
    let at = |id: &str| order.iter().position(|k| k.as_str() == id).unwrap();
    at(first) < at(second)
}

/// Scenario: On one pane, generation A exits and B takes the pane; then B exits holding typed keys while A's already-retired record still lingers, and C is spawned onto the pane, in fresh registries until the publish has visited A before B and B before A. Each time C's card reports B's discarded typing: A's cleanup, retiring nothing, never erases it (R-E-U2-01).
#[spec("inbox/probe/082")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_082_an_already_retired_record_never_erases_its_successors_notice() {
    let pane = "probe-082-repeated-replacement";
    let mut seen = (false, false);
    for round in 0..64 {
        let registry = Arc::new(AgentPtyRegistry::new());
        let a = spawn_exited(&registry, pane).await;
        let b = spawn_exited(&registry, pane).await;
        assert!(
            registry.agent_record_any(&a).is_some(),
            "precondition: A's retired record lingers on the pane"
        );
        hold_typing(&registry, &b, b"B-TYPED");
        let a_first = visits_first(&registry, &a, &b);
        let c = registry
            .spawn_agent(opts(pane, "/bin/sh"))
            .expect("C takes the pane");
        let shown = registry.input_discarded(&c);
        assert_eq!(
            shown
                .as_ref()
                .map(|d| (d.generation, d.held_ops, d.held_bytes)),
            Some((b.parse().unwrap(), 1, 7)),
            "round {round}, A visited {}: C's card must report B's typing",
            if a_first { "first" } else { "second" },
        );
        registry.shutdown_all();
        if a_first {
            seen.0 = true;
        } else {
            seen.1 = true;
        }
        if seen == (true, true) {
            return;
        }
    }
    panic!("64 registries never visited both orders: {seen:?}");
}

/// Scenario: Generation A exits holding typed keys and B takes the pane, so B's card says A's typing wasn't delivered; then A's lingering record is closed. B's card still says so: closing an older record that retired long ago leaves its successor's notice (R-E-U2-01).
#[spec("inbox/probe/082")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_082_closing_an_old_predecessor_keeps_the_successors_notice() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "probe-082-close-predecessor";
    let a = spawn_exited(&registry, pane).await;
    hold_typing(&registry, &a, b"A-TYPED");
    let b = registry
        .spawn_agent(opts(pane, "/bin/sh"))
        .expect("B takes the pane");
    let generation: u64 = a.parse().unwrap();
    assert_eq!(
        registry.input_discarded(&b).map(|d| d.generation),
        Some(generation),
        "precondition: B shows A's notice"
    );
    registry
        .close_agent(&a)
        .expect("close A's lingering record");
    assert_eq!(
        registry.input_discarded(&b).map(|d| d.generation),
        Some(generation),
        "closing the already-retired predecessor must not clear B's notice"
    );
    registry.shutdown_all();
}

/// Scenario: Generation A is stopped while holding typed keys and B is then spawned onto the same pane, as `probe_046` replaces a generation; B shows the notice, and so does C after B retires holding nothing. A person's keystroke on the pane ends it and the registry lets the entry go (auditor U3-1, U3-2, R-E-U3-02).
#[spec("inbox/probe/082")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_082_the_notice_lasts_until_a_keystroke_and_then_is_pruned() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "probe-082-lifetime";
    let a = spawn_exited(&registry, pane).await;
    hold_typing(&registry, &a, b"A-TYPED");
    registry.close_agent(&a).expect("stop A");
    assert_eq!(
        registry.discard_panes(),
        vec![pane.to_string()],
        "a stop keeps the notice for a successor on the same pane"
    );
    let generation: u64 = a.parse().unwrap();
    let b = spawn_exited(&registry, pane).await;
    assert_eq!(
        registry.input_discarded(&b).map(|d| d.generation),
        Some(generation),
        "the successor spawned after the stop shows A's notice"
    );
    let c = registry
        .spawn_agent(opts(pane, "/bin/sh"))
        .expect("C takes the pane");
    assert_eq!(
        registry.input_discarded(&c).map(|d| d.generation),
        Some(generation),
        "B retired holding nothing: no keystroke yet, so A's notice stands"
    );
    registry.note_user_input(pane);
    assert_eq!(registry.input_discarded(&c), None, "a keystroke ends it");
    assert!(
        registry.discard_panes().is_empty(),
        "an ended notice is pruned"
    );
    registry.shutdown_all();
}

/// Scenario: Retirements that held typing are recorded on one more pane than the registry keeps notices for. The oldest retirement's notice is the one let go, and every later one stays (R-E-U3-02).
#[spec("inbox/probe/082")]
#[test]
fn probe_082_the_notices_are_bounded_oldest_first() {
    let registry = AgentPtyRegistry::new();
    for n in 0..=MAX_PANE_DISCARDS {
        // Each retirement after a keystroke elsewhere, so each is younger.
        registry.note_user_input("probe-082-elsewhere");
        let mut inner = registry.inner.lock();
        registry.note_discard(
            &mut inner,
            &format!("probe-082-cap-{n:03}"),
            &(n + 1).to_string(),
            Some((1, 1)),
        );
    }
    let kept = registry.discard_panes();
    assert_eq!(kept.len(), MAX_PANE_DISCARDS);
    assert!(
        !kept.contains(&"probe-082-cap-000".to_string()),
        "the oldest goes: {kept:?}"
    );
    assert!(kept.contains(&format!("probe-082-cap-{MAX_PANE_DISCARDS:03}")));
}

/// Scenario: Retirements that held typing are recorded on one more pane than the registry keeps notices for, with no keystroke between any of them and the later ones naming lower generations than the first. The first retirement's notice is the one let go, and the one just recorded stays: retirement order, not the generation number, decides which is oldest (review U3-d).
#[spec("inbox/probe/082")]
#[test]
fn probe_082_the_bound_lets_go_of_the_oldest_retirement_not_the_lowest_generation() {
    let registry = AgentPtyRegistry::new();
    let record = |n: usize, generation: usize| {
        let mut inner = registry.inner.lock();
        registry.note_discard(
            &mut inner,
            &format!("probe-082-tie-{n:03}"),
            &generation.to_string(),
            Some((1, 1)),
        );
    };
    // The first retirement names the highest generation, and the clock moves
    // on before the rest, so its `at_ms` is the oldest.
    record(0, 10_000);
    std::thread::sleep(Duration::from_millis(5));
    for n in 1..=MAX_PANE_DISCARDS {
        record(n, 10_000 - n);
    }
    let kept = registry.discard_panes();
    assert_eq!(kept.len(), MAX_PANE_DISCARDS);
    assert!(
        !kept.contains(&"probe-082-tie-000".to_string()),
        "the first retirement goes: {kept:?}"
    );
    assert!(
        kept.contains(&format!("probe-082-tie-{MAX_PANE_DISCARDS:03}")),
        "the one just recorded stays: {kept:?}"
    );
}

/// Scenario: The registry holds as many notices as it keeps, recorded with no keystroke between them, and one more retirement that held typing is recorded. The first retirement's notice is the one let go, both when every held notice was stamped in one millisecond and when the wall clock stepped back before the last one (review U3-d's tie).
#[spec("inbox/probe/082")]
#[test]
fn probe_082_the_bound_holds_retirement_order_within_one_millisecond() {
    let fill = |registry: &AgentPtyRegistry, at_ms: u64| {
        for n in 0..MAX_PANE_DISCARDS {
            let mut inner = registry.inner.lock();
            registry.note_discard(
                &mut inner,
                &format!("probe-082-ms-{n:03}"),
                &(n + 1).to_string(),
                Some((1, 1)),
            );
        }
        // Stamp every held notice with one wall-clock millisecond: an
        // instant before the next retirement's, or after it once the clock
        // steps back.
        let mut inner = registry.inner.lock();
        for discard in inner.input_discarded.values_mut() {
            discard.shown.at_ms = at_ms;
        }
    };
    let last = format!("probe-082-ms-{MAX_PANE_DISCARDS:03}");
    let record_last = |registry: &AgentPtyRegistry| {
        let mut inner = registry.inner.lock();
        registry.note_discard(&mut inner, &last, "999", Some((1, 1)));
    };
    // A tie is broken in whatever order a map visits its entries, which
    // differs per map, so several registries each get a chance to show it.
    for round in 0..16 {
        let registry = AgentPtyRegistry::new();
        fill(&registry, 0);
        record_last(&registry);
        let kept = registry.discard_panes();
        assert_eq!(kept.len(), MAX_PANE_DISCARDS);
        assert!(
            !kept.contains(&"probe-082-ms-000".to_string()),
            "round {round}: one millisecond, the first retirement goes: {kept:?}"
        );
    }
    let registry = AgentPtyRegistry::new();
    fill(&registry, u64::MAX);
    record_last(&registry);
    let kept = registry.discard_panes();
    assert_eq!(kept.len(), MAX_PANE_DISCARDS);
    assert!(
        !kept.contains(&"probe-082-ms-000".to_string()),
        "the clock stepped back, the first retirement goes: {kept:?}"
    );
    assert!(
        kept.contains(&last),
        "the clock stepped back, the one just recorded stays: {kept:?}"
    );
}

/// Scenario: Generation A exits holding only a voice answer's keys, and B takes the pane: B's card reports no not-delivered typing. Then A2 exits holding the person's typed keys beside an answer, and B2 takes its pane: B2's card counts the typing alone (auditor U3-7).
#[spec("inbox/probe/082")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_082_a_held_answer_is_not_reported_as_held_typing() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "probe-082-answer-only";
    let a = spawn_exited(&registry, pane).await;
    hold_answer(&registry, &a, b"1");
    let b = spawn_exited(&registry, pane).await;
    assert_eq!(
        registry.input_discarded(&b),
        None,
        "an answer-only hold is no person's typing"
    );
    let pane = "probe-082-answer-and-typing";
    let a2 = spawn_exited(&registry, pane).await;
    hold_typing(&registry, &a2, b"TYPED");
    hold_answer(&registry, &a2, b"2");
    let b2 = registry
        .spawn_agent(opts(pane, "/bin/sh"))
        .expect("B2 takes the pane");
    assert_eq!(
        registry
            .input_discarded(&b2)
            .map(|d| (d.held_ops, d.held_bytes)),
        Some((1, 5)),
        "only the typed write is counted"
    );
    registry.shutdown_all();
}
