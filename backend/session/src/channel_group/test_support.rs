use shared::model::{PlaylistItemType, VirtualId, XtreamCluster};
use tuliprox_repository::GroupMember;

pub fn test_member(input: &str, stream_id: &str, provider_id: u32, member_vid: u32) -> GroupMember {
    GroupMember {
        member_vid: VirtualId::new(member_vid),
        input_name: input.into(),
        provider_id,
        input_stream_id: stream_id.into(),
        url: format!("http://{input}.invalid/{stream_id}.ts").into(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        title: "ZDF".into(),
        archive: None,
        epg_channel_id: None,
        additional_properties: None,
    }
}
