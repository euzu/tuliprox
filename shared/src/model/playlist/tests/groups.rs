use super::*;

#[test]
fn from_stalker_populates_input_name_and_group() {
    let item = StalkerPlaylistItem {
        stream_id: 7,
        stream_kind: StalkerStreamKind::Movie,
        category_name: Internable::intern("Action".to_string()),
        cmd: Internable::intern("ffmpeg http://streams.example/movie/7".to_string()),
        ..StalkerPlaylistItem::default()
    };
    let converted = PlaylistItem::from_stalker(&item, "my_input");
    assert_eq!(&*converted.header.input_name, "my_input");
    assert_eq!(&*converted.header.input_stream_id, "7");
    assert!(converted.header.url.is_empty());
    assert_eq!(&*converted.header.group, "Action");
    assert_eq!(converted.header.item_type, PlaylistItemType::Video);
}

#[test]
fn from_stalker_group_falls_back_to_cluster_default() {
    let item =
        StalkerPlaylistItem { stream_id: 7, stream_kind: StalkerStreamKind::Movie, ..StalkerPlaylistItem::default() };
    let converted = PlaylistItem::from_stalker(&item, "my_input");
    assert_eq!(&*converted.header.group, "Movies");
}
