use super::*;

#[test]
fn duplicate_readiness_evidence_is_not_accepted_as_safe_base() {
    let mut input = build_input(1, manifest(), asset());
    let duplicate = input.base_availability[1];
    let mut states = input.base_availability.to_vec();
    states.push(duplicate);
    input.base_availability = Arc::from(states);

    assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
}
