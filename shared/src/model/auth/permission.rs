use crate::create_bitset;

create_bitset!(
    u32,
    Permission,
    ConfigRead,
    ConfigWrite,
    SourceRead,
    SourceWrite,
    UserRead,
    UserWrite,
    PlaylistRead,
    PlaylistWrite,
    LibraryRead,
    LibraryWrite,
    SystemRead,
    SystemWrite,
    EpgRead,
    EpgWrite,
    RecordingRead,
    RecordingCreate,
    RecordingManage,
    RecordingDelete
);

pub const PERM_ALL: PermissionSet = PermissionSet::ALL;

pub const PERMISSION_NAMES: &[(&str, Permission)] = &[
    ("config.read", Permission::ConfigRead),
    ("config.write", Permission::ConfigWrite),
    ("source.read", Permission::SourceRead),
    ("source.write", Permission::SourceWrite),
    ("user.read", Permission::UserRead),
    ("user.write", Permission::UserWrite),
    ("playlist.read", Permission::PlaylistRead),
    ("playlist.write", Permission::PlaylistWrite),
    ("library.read", Permission::LibraryRead),
    ("library.write", Permission::LibraryWrite),
    ("system.read", Permission::SystemRead),
    ("system.write", Permission::SystemWrite),
    ("epg.read", Permission::EpgRead),
    ("epg.write", Permission::EpgWrite),
    ("recording.read", Permission::RecordingRead),
    ("recording.create", Permission::RecordingCreate),
    ("recording.manage", Permission::RecordingManage),
    ("recording.delete", Permission::RecordingDelete),
];

pub fn permission_from_name(name: &str) -> Option<Permission> {
    PERMISSION_NAMES.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

pub fn permission_to_name(perm: Permission) -> Option<&'static str> {
    PERMISSION_NAMES.iter().find(|(_, p)| *p == perm).map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_set_new_is_empty() {
        let set = PermissionSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(Permission::ConfigRead));
    }

    #[test]
    fn test_permission_set_set_and_contains() {
        let mut set = PermissionSet::new();
        set.set(Permission::ConfigRead);
        assert!(set.contains(Permission::ConfigRead));
        assert!(!set.contains(Permission::ConfigWrite));
        assert!(!set.is_empty());
    }

    #[test]
    fn test_permission_set_from_variants() {
        let set = Permission::ConfigRead | Permission::SourceRead;
        assert!(set.contains(Permission::ConfigRead));
        assert!(set.contains(Permission::SourceRead));
        assert!(!set.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_permission_set_union() {
        let mut a: PermissionSet = Permission::ConfigRead.into();
        let b: PermissionSet = Permission::SourceRead.into();
        a.union(b);
        assert!(a.contains(Permission::ConfigRead));
        assert!(a.contains(Permission::SourceRead));
    }

    #[test]
    fn test_permission_set_bitor() {
        let a: PermissionSet = Permission::ConfigRead.into();
        let b: PermissionSet = Permission::SourceRead.into();
        let c = a | b;
        assert!(c.contains(Permission::ConfigRead));
        assert!(c.contains(Permission::SourceRead));
    }

    #[test]
    fn test_permission_set_unset() {
        let mut set = Permission::ConfigRead | Permission::ConfigWrite;
        set.unset(Permission::ConfigRead);
        assert!(!set.contains(Permission::ConfigRead));
        assert!(set.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_perm_all_contains_every_permission() {
        assert!(PERM_ALL.contains(Permission::ConfigRead));
        assert!(PERM_ALL.contains(Permission::ConfigWrite));
        assert!(PERM_ALL.contains(Permission::EpgRead));
        assert!(PERM_ALL.contains(Permission::EpgWrite));
        assert!(PERM_ALL.contains(Permission::RecordingRead));
        assert!(PERM_ALL.contains(Permission::RecordingCreate));
        assert!(PERM_ALL.contains(Permission::RecordingManage));
        assert!(PERM_ALL.contains(Permission::RecordingDelete));
    }

    #[test]
    fn test_perm_all_matches_defined_permissions_only() {
        // All defined bits set; no trailing zeros and no overflow.
        let expected_mask: u32 = if PermissionSet::VARIANT_COUNT == u32::BITS as usize {
            u32::MAX
        } else {
            (1u32 << PermissionSet::VARIANT_COUNT) - 1
        };
        assert_eq!(PERM_ALL.0, expected_mask);
    }

    #[test]
    fn download_permission_names_are_gone_and_decode_to_nothing() {
        // The groups file stores permission *names*. A file that still
        // lists the removed download permissions must lose them, not have
        // them silently reinterpreted as the recording ones.
        assert_eq!(permission_from_name("download.read"), None);
        assert_eq!(permission_from_name("download.write"), None);
        assert!(PERMISSION_NAMES.iter().all(|(name, _)| !name.starts_with("download.")));
    }

    #[test]
    fn the_removed_recording_write_name_decodes_to_nothing() {
        // Splitting the single write permission renumbered the bits above it.
        // A groups file that still lists the old name must lose it rather than
        // silently gain one of the three replacements.
        assert_eq!(permission_from_name("recording.write"), None);
    }

    #[test]
    fn permission_bits_are_frozen() {
        // Reordering the enum would reinterpret every issued token. These
        // values are the wire format; changing one requires bumping
        // `CURRENT_PERMISSION_SCHEMA_VERSION` in lockstep.
        let expected: &[(&str, u32)] = &[
            ("config.read", 1 << 0),
            ("config.write", 1 << 1),
            ("source.read", 1 << 2),
            ("source.write", 1 << 3),
            ("user.read", 1 << 4),
            ("user.write", 1 << 5),
            ("playlist.read", 1 << 6),
            ("playlist.write", 1 << 7),
            ("library.read", 1 << 8),
            ("library.write", 1 << 9),
            ("system.read", 1 << 10),
            ("system.write", 1 << 11),
            ("epg.read", 1 << 12),
            ("epg.write", 1 << 13),
            ("recording.read", 1 << 14),
            ("recording.create", 1 << 15),
            ("recording.manage", 1 << 16),
            ("recording.delete", 1 << 17),
        ];
        assert_eq!(PermissionSet::VARIANT_COUNT, expected.len());
        for (name, bit) in expected {
            let permission = permission_from_name(name).expect("permission name is missing");
            let set: PermissionSet = permission.into();
            assert_eq!(set.0, *bit, "bit value of {name} changed");
        }
    }

    #[test]
    fn test_permission_from_name() {
        assert_eq!(permission_from_name("config.read"), Some(Permission::ConfigRead));
        assert_eq!(permission_from_name("source.write"), Some(Permission::SourceWrite));
        assert_eq!(permission_from_name("recording.read"), Some(Permission::RecordingRead));
        assert_eq!(permission_from_name("recording.create"), Some(Permission::RecordingCreate));
        assert_eq!(permission_from_name("recording.manage"), Some(Permission::RecordingManage));
        assert_eq!(permission_from_name("recording.delete"), Some(Permission::RecordingDelete));
        assert_eq!(permission_from_name("nonexistent"), None);
        assert_eq!(permission_from_name(""), None);
    }

    #[test]
    fn test_permission_to_name() {
        assert_eq!(permission_to_name(Permission::ConfigRead), Some("config.read"));
        assert_eq!(permission_to_name(Permission::EpgWrite), Some("epg.write"));
        assert_eq!(permission_to_name(Permission::RecordingRead), Some("recording.read"));
        assert_eq!(permission_to_name(Permission::RecordingCreate), Some("recording.create"));
        assert_eq!(permission_to_name(Permission::RecordingManage), Some("recording.manage"));
        assert_eq!(permission_to_name(Permission::RecordingDelete), Some("recording.delete"));
    }

    #[test]
    fn test_permission_set_is_subset_of() {
        let small: PermissionSet = Permission::ConfigRead.into();
        let large = Permission::ConfigRead | Permission::SourceRead;
        assert!(small.is_subset_of(&large));
        assert!(!large.is_subset_of(&small));
    }

    #[test]
    fn test_permission_set_serde_roundtrip() {
        let set = Permission::ConfigRead | Permission::SourceWrite | Permission::RecordingRead;
        let json = serde_json::to_string(&set).expect("serialize failed");
        let deserialized: PermissionSet = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(set, deserialized);
    }

    #[test]
    fn test_permission_set_default_is_zero() {
        let set: PermissionSet = Default::default();
        assert!(set.is_empty());
        assert_eq!(set.0, 0);
    }
}
