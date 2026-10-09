use super::{EventKind, EventKindMask};

#[test]
fn every_kind_has_a_distinct_bit() {
    let mut seen = 0u64;
    for kind in EventKind::ALL {
        assert_eq!(seen & kind.bit(), 0, "{kind:?} shares a bit with an earlier kind");
        seen |= kind.bit();
    }
}

/// `EventKindMask` is a `u64`, so the taxonomy cannot outgrow 64 kinds
/// without the mask type changing with it. It was a `u32` at 23 kinds;
/// the widening happened while no operator had a subscription list to
/// migrate, which is the only cheap time to do it.
#[test]
fn the_taxonomy_still_fits_in_the_mask() {
    assert!(EventKind::ALL.len() <= 64, "EventKindMask needs a wider integer");
}

#[test]
fn wire_names_are_unique_and_round_trip() {
    let mut names: Vec<&str> = EventKind::ALL.iter().map(|kind| kind.as_wire_name()).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "two kinds share a wire name");

    for kind in EventKind::ALL {
        assert_eq!(EventKind::from_wire_name(kind.as_wire_name()), Some(kind));
    }
}

#[test]
fn an_unknown_subscription_name_is_reported_not_ignored() {
    let (mask, unknown) = EventKindMask::from_wire_names(["playlist.update", "playlist.updat", "config.changed"]);

    assert!(mask.contains(EventKind::PlaylistUpdate));
    assert!(mask.contains(EventKind::ConfigChange));
    assert_eq!(unknown, vec!["playlist.updat"], "a typo must surface, not silently narrow the subscription");
}

#[test]
fn a_mask_round_trips_through_its_kinds() {
    let mask = EventKindMask::from_iter([EventKind::ServerError, EventKind::RecordingChanged]);
    assert_eq!(mask.kinds(), vec![EventKind::ServerError, EventKind::RecordingChanged]);
    assert!(!mask.contains(EventKind::SystemInfoUpdate));
    assert!(!mask.is_empty());
    assert!(EventKindMask::NONE.is_empty());
}

#[test]
fn all_matches_every_kind() {
    for kind in EventKind::ALL {
        assert!(EventKindMask::ALL.contains(kind));
    }
}
