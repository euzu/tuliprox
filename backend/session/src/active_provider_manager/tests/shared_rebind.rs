use super::{
    super::{ActiveProviderManager, ConnectionKind},
    capacity_probe::{addr, manager_with_inputs},
};
use std::sync::Arc;
use tuliprox_core::model::SharedSubscriberId;

fn slots(manager: &ActiveProviderManager, input: &str) -> usize {
    manager.active_connections().unwrap_or_default().get(input).copied().unwrap_or_default()
}

const KEY: &str = "group:1:7";

#[test]
fn detach_then_attach_moves_shared_allocation_to_new_provider() {
    let manager = manager_with_inputs(&[("solis", 2), ("korath", 2)]);
    let solis: Arc<str> = "solis".into();
    let korath: Arc<str> = "korath".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    manager
        .add_shared_connection(&addr(2), SharedSubscriberId::from_stream_uid(2), KEY, 0, ConnectionKind::Normal)
        .expect("join");

    assert!(manager.detach_shared_allocation(KEY, first.allocation_id));
    assert_eq!(slots(&manager, "solis"), 0);
    assert!(!manager.detach_shared_allocation(KEY, first.allocation_id), "a detached stream has nothing to release");

    let next = manager.acquire_connection(&korath, &addr(1), 0, ConnectionKind::Normal).expect("korath slot");
    assert!(manager.attach_shared_allocation(KEY, first.allocation_id, &next));
    assert_eq!((slots(&manager, "solis"), slots(&manager, "korath")), (0, 1));

    manager.release_shared_connection(SharedSubscriberId::from_stream_uid(2));
    assert_eq!(slots(&manager, "korath"), 1, "one subscriber still watches");
    manager.release_shared_connection(SharedSubscriberId::from_stream_uid(1));
    assert_eq!(slots(&manager, "korath"), 0);
}

#[test]
fn release_handle_after_attach_releases_new_allocation() {
    let manager = manager_with_inputs(&[("solis", 1), ("korath", 1)]);
    let solis: Arc<str> = "solis".into();
    let korath: Arc<str> = "korath".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    manager.detach_shared_allocation(KEY, first.allocation_id);
    let next = manager.acquire_connection(&korath, &addr(1), 0, ConnectionKind::Normal).expect("korath slot");
    assert!(manager.attach_shared_allocation(KEY, first.allocation_id, &next));
    manager.release_handle(&first);
    assert_eq!(slots(&manager, "korath"), 1, "the old handle no longer owns the shared stream");
    manager.release_handle(&next);
    assert_eq!(slots(&manager, "korath"), 0);
}

#[test]
fn attach_to_unknown_key_keeps_single_allocation() {
    let manager = manager_with_inputs(&[("korath", 1)]);
    let korath: Arc<str> = "korath".into();
    let handle = manager.acquire_connection(&korath, &addr(1), 0, ConnectionKind::Normal).expect("korath slot");
    assert!(!manager.attach_shared_allocation(KEY, 0, &handle));
    assert_eq!(slots(&manager, "korath"), 1);
    manager.release_handle(&handle);
    assert_eq!(slots(&manager, "korath"), 0);
}

#[test]
fn attach_keeps_session_owner_of_shared_stream() {
    let manager = manager_with_inputs(&[("solis", 1), ("korath", 1)]);
    let solis: Arc<str> = "solis".into();
    let korath: Arc<str> = "korath".into();
    let first = manager
        .try_acquire_connection_no_preempt(
            &solis,
            false,
            &addr(1),
            0,
            ConnectionKind::Normal,
            Some(super::super::PlaybackLeaseRef::for_session_owner("viewer-session")),
        )
        .expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(manager.has_active_owner_for_provider(&solis, "viewer-session"));
    manager.detach_shared_allocation(KEY, first.allocation_id);
    let next = manager.acquire_connection(&korath, &addr(1), 0, ConnectionKind::Normal).expect("korath slot");
    assert!(manager.attach_shared_allocation(KEY, first.allocation_id, &next));
    assert!(manager.has_active_owner_for_provider(&korath, "viewer-session"));
}

#[test]
fn detach_with_stale_allocation_id_is_refused() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(!manager.detach_shared_allocation(KEY, first.allocation_id + 1));
    assert_eq!(slots(&manager, "solis"), 1, "the running allocation stays");
}

#[test]
fn attach_after_foreign_detach_is_refused() {
    let manager = manager_with_inputs(&[("solis", 1), ("korath", 1)]);
    let solis: Arc<str> = "solis".into();
    let korath: Arc<str> = "korath".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(manager.detach_shared_allocation(KEY, first.allocation_id));
    let next = manager.acquire_connection(&korath, &addr(1), 0, ConnectionKind::Normal).expect("korath slot");
    assert!(!manager.attach_shared_allocation(KEY, first.allocation_id + 1, &next));
    assert_eq!(slots(&manager, "korath"), 1, "the refused handle stays a single allocation");
    manager.release_handle(&next);
    assert_eq!(slots(&manager, "korath"), 0);
}

#[test]
fn join_during_detach_leaves_no_priority_entry() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(manager.detach_shared_allocation(KEY, first.allocation_id));
    manager
        .add_shared_connection(&addr(2), SharedSubscriberId::from_stream_uid(2), KEY, -5, ConnectionKind::Normal)
        .expect("join while detached");
    let connections = manager.read_connections();
    assert!(connections.priority_index.values().all(std::collections::BTreeMap::is_empty));
}

#[test]
fn teardown_in_detach_window_leaves_no_entry() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    let first = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(manager.detach_shared_allocation(KEY, first.allocation_id));
    manager.release_shared_connection(SharedSubscriberId::from_stream_uid(1));
    manager.release_handle(&first);
    let connections = manager.read_connections();
    assert!(connections.shared.by_key.is_empty() && connections.shared.shared_by_allocation_id.is_empty());
    drop(connections);
    assert_eq!(slots(&manager, "solis"), 0);
}

#[test]
fn preempt_after_attach_targets_new_allocation() {
    let manager = manager_with_inputs(&[("solis", 1), ("korath", 1)]);
    let solis: Arc<str> = "solis".into();
    let korath: Arc<str> = "korath".into();
    let first = manager.acquire_connection(&solis, &addr(1), 10, ConnectionKind::Normal).expect("solis slot");
    assert!(manager.make_shared_connection(&first, KEY, SharedSubscriberId::from_stream_uid(1)));
    assert!(manager.detach_shared_allocation(KEY, first.allocation_id));
    let next = manager.acquire_connection(&korath, &addr(1), 10, ConnectionKind::Normal).expect("korath slot");
    assert!(manager.attach_shared_allocation(KEY, first.allocation_id, &next));
    let connections = manager.read_connections();
    let entries: Vec<_> =
        connections.priority_index.get(&korath).map(|tree| tree.values().copied().collect()).unwrap_or_default();
    assert!(
        entries
            .iter()
            .any(|owner| matches!(owner, super::super::PriorityOwner::Shared(id) if *id == next.allocation_id)),
        "preemption finds the shared stream under its new allocation"
    );
    assert!(connections.priority_index.get(&solis).is_none_or(std::collections::BTreeMap::is_empty));
}
