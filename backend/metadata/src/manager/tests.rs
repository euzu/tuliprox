use super::*;

/// Run the `collect_*_virtual_updates` text-id resolution test for any
/// cluster (vod / series / live). The macro emits the shared tempdir +
/// mapping + batch + collect + assert scaffolding; the cluster-specific
/// bits (`$fn_name`, `$item_type`, `$input_name`, `$text_id`,
/// `$add_method`, `$props`) are passed by the caller.
macro_rules! assert_collect_resolves_text_id {
    ($fn_name:ident, $item_type:expr, $input_name:expr, $text_id:expr, $add_method:ident, $props:expr) => {
        let dir = tempdir().expect("tempdir should be created");
        let mapping_path = dir.path().join("target_id_mapping.db");
        let mut mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping should be created");
        let uuid = generate_provider_playlist_uuid($input_name, $text_id, $item_type);
        let virtual_id = mapping.get_and_update_virtual_id(&uuid, 0, $item_type, VirtualId::default());
        mapping.persist().expect("mapping should persist");

        let mut batch = BatchResultCollector::new();
        batch.$add_method(ProviderIdType::from($text_id), $props);

        let mut provider_virtual_ids = HashMap::new();
        let mut uuid_virtual_ids = HashMap::new();
        let updates =
            InputWorker::$fn_name(&mapping, $input_name, &batch, &mut provider_virtual_ids, &mut uuid_virtual_ids);

        assert!(updates.contains_key(&virtual_id));
    };
}

mod behavior;

mod lifecycle;

mod retry;

mod support;

use self::support::create_test_worker;
