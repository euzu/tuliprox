use super::*;
use shared::model::{PlaylistItemType, XtreamCluster};

const GROUP_VID: VirtualId = VirtualId(100);

fn member(vid: u32, stream_id: &str) -> GroupMember {
    GroupMember {
        member_vid: VirtualId(vid),
        input_name: Arc::from("a"),
        provider_id: 0,
        input_stream_id: Arc::from(stream_id),
        url: Arc::from("http://provider.test/live"),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        title: Arc::from("ZDF"),
        archive: None,
        epg_channel_id: None,
        additional_properties: None,
    }
}

fn record() -> FailoverGroupRecord {
    FailoverGroupRecord {
        group_vid: GROUP_VID,
        key: Arc::from("zdf"),
        origin: FailoverGroupOrigin::Auto,
        policy: FailoverGroupPolicy::Auto,
        members: vec![member(1, "1"), member(2, "2"), member(3, "3")],
        orphaned: Vec::new(),
    }
}

fn applied_meta(etag: &str) -> FailoverGroupMeta {
    FailoverGroupMeta {
        overrides_etag: Some(Arc::from(etag)),
        applied_overrides: Some(TargetFailoverOverridesDto::new("t")),
        ..FailoverGroupMeta::default()
    }
}

fn store_with(meta: &FailoverGroupMeta) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    FailoverGroupStore::write(dir.path(), &[record()], &[], Some(FailoverGroupSideFiles { catalog: &[], meta }))
        .expect("store written");
    dir
}

fn reorder() -> Vec<HotApplyGroupChange> {
    vec![HotApplyGroupChange {
        group_vid: GROUP_VID,
        member_order: vec![VirtualId(3), VirtualId(1), VirtualId(2)],
        policy: FailoverGroupPolicy::Priority,
    }]
}

fn stored_order(dir: &Path) -> Vec<VirtualId> {
    let stored = FailoverGroupStore::read_group(dir, GROUP_VID).expect("read").expect("group");
    stored.members.iter().map(|member| member.member_vid).collect()
}

#[test]
fn hot_change_writes_groups_then_final_meta() {
    let dir = store_with(&applied_meta("etag-1"));
    let next = FailoverGroupMeta { display_stale_groups: vec!["zdf".to_string()], ..applied_meta("etag-2") };

    FailoverGroupStore::apply_hot_change(dir.path(), &reorder(), &next).expect("applies");

    assert_eq!(stored_order(dir.path()), [VirtualId(3), VirtualId(1), VirtualId(2)]);
    assert_eq!(FailoverGroupStore::read_meta(dir.path()).expect("meta"), next);
}

#[test]
fn failure_after_the_groups_write_leaves_the_meta_invalidated() {
    let dir = store_with(&applied_meta("etag-1"));
    let next = FailoverGroupMeta { display_stale_groups: vec!["zdf".to_string()], ..applied_meta("etag-2") };

    let result = FailoverGroupStore::apply_hot_change_with(dir.path(), &reorder(), &next, |_, _| {
        Err(std::io::Error::other("injected meta write failure"))
    });

    assert!(result.is_err());
    // The groups are already reordered, so the meta must not claim they still match the old section.
    assert_eq!(stored_order(dir.path()), [VirtualId(3), VirtualId(1), VirtualId(2)]);
    let meta = FailoverGroupStore::read_meta(dir.path()).expect("meta");
    assert_eq!(meta.overrides_etag, None);
    assert_eq!(meta.applied_overrides, None);
    assert_eq!(meta.display_stale_groups, ["zdf"]);
}

#[test]
fn failed_groups_write_leaves_the_meta_invalidated_and_groups_untouched() {
    let dir = store_with(&applied_meta("etag-1"));
    let unknown = vec![HotApplyGroupChange {
        group_vid: GROUP_VID,
        member_order: vec![VirtualId(1), VirtualId(2), VirtualId(9)],
        policy: FailoverGroupPolicy::Auto,
    }];

    assert!(FailoverGroupStore::apply_hot_change(dir.path(), &unknown, &applied_meta("etag-2")).is_err());

    assert_eq!(stored_order(dir.path()), [VirtualId(1), VirtualId(2), VirtualId(3)]);
    let meta = FailoverGroupStore::read_meta(dir.path()).expect("meta");
    assert_eq!((meta.overrides_etag, meta.applied_overrides), (None, None));
}

#[test]
fn etag_only_change_writes_just_the_meta() {
    let dir = store_with(&applied_meta("etag-1"));

    FailoverGroupStore::apply_hot_change_with(dir.path(), &[], &applied_meta("etag-2"), |path, meta| {
        // No invalidation step: the meta on disk is still the old one when the final write runs.
        assert_eq!(FailoverGroupStore::read_meta(path).expect("meta").overrides_etag.as_deref(), Some("etag-1"));
        FailoverGroupStore::write_meta(path, meta)
    })
    .expect("applies");

    assert_eq!(FailoverGroupStore::read_meta(dir.path()).expect("meta").overrides_etag.as_deref(), Some("etag-2"));
}

#[test]
fn member_vid_resolves_by_input_and_stream_id() {
    let record = record();
    let find = |input: &str, stream_id: &str| {
        record.member_vid(&ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() })
    };
    assert_eq!(find("a", "2"), Some(VirtualId(2)));
    assert_eq!(find("b", "2"), None);
    assert_eq!(find("a", "9"), None);
}
