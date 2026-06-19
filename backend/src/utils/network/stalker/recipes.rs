use shared::model::stalker::{
    StalkerAuthMode, StalkerBootstrapRecipe, StalkerEndpointPreference, StalkerMagPreset, StalkerPortalFingerprint,
};

use crate::utils::network::stalker::presets::stalker_mag_preset_spec;

/// A recipe captures "how to perform the handshake against a portal of flavour X using
/// auth mode Y and MAG preset Z". The handshake is a sequence of HTTP calls (`handshake`,
/// optionally `handshake-extra`/`handshake-check`/`portal-info`) and each recipe fixes the
/// order, the parameters and the failure semantics for that call set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct StalkerRecipeSpec {
    pub recipe: StalkerBootstrapRecipe,
    /// Whether to call `/server/load.php?type=stb&action=handshake-extra` after the main
    /// handshake call. Strict MAG portals require this; loose portals do not.
    pub emit_handshake_extra: bool,
    /// Whether the cookie returned by the main handshake should be re-issued through the
    /// `/portal.php?type=stb&action=handshake` endpoint to bind the session server-side.
    pub require_portal_handshake: bool,
    /// Whether `create_link` calls should send the bearer token only as `Authorization`
    /// (true) or also as a `token` query parameter (false).
    pub token_in_query: bool,
}

/// Default recipes per `(auth_mode, preset)` tuple. The table is intentionally small —
/// runtime fallback order is derived in `fallback_recipes_for` from the same enum space.
pub fn default_recipe_for(auth_mode: StalkerAuthMode, preset: StalkerMagPreset) -> StalkerRecipeSpec {
    let _ = preset;
    match auth_mode {
        StalkerAuthMode::Auto | StalkerAuthMode::MacOnly => StalkerRecipeSpec {
            recipe: StalkerBootstrapRecipe::GenericSafe,
            emit_handshake_extra: false,
            require_portal_handshake: false,
            token_in_query: false,
        },
        StalkerAuthMode::CredentialsOnly => StalkerRecipeSpec {
            recipe: StalkerBootstrapRecipe::AuthOnly,
            emit_handshake_extra: false,
            require_portal_handshake: false,
            token_in_query: false,
        },
        StalkerAuthMode::MacPlusCredentials => StalkerRecipeSpec {
            recipe: StalkerBootstrapRecipe::AuthStrictMag,
            emit_handshake_extra: true,
            require_portal_handshake: true,
            token_in_query: true,
        },
    }
}

/// The ordered list of recipes to try when the initial one is rejected. We always end the
/// list with the original `default_recipe_for` value so we eventually re-try with what
/// the user actually configured.
pub fn fallback_recipes_for(auth_mode: StalkerAuthMode, preset: StalkerMagPreset) -> Vec<StalkerBootstrapRecipe> {
    let default = default_recipe_for(auth_mode, preset).recipe;
    let mut chain: Vec<StalkerBootstrapRecipe> = match auth_mode {
        StalkerAuthMode::Auto | StalkerAuthMode::MacOnly => vec![
            StalkerBootstrapRecipe::GenericSafe,
            StalkerBootstrapRecipe::StrictMag,
            StalkerBootstrapRecipe::LegacyMag,
            StalkerBootstrapRecipe::PortalPreferred,
            StalkerBootstrapRecipe::LocalizationStrict,
        ],
        StalkerAuthMode::CredentialsOnly => vec![
            StalkerBootstrapRecipe::AuthOnly,
            StalkerBootstrapRecipe::AuthStrictMag,
            StalkerBootstrapRecipe::ModuleGated,
        ],
        StalkerAuthMode::MacPlusCredentials => vec![
            StalkerBootstrapRecipe::AuthStrictMag,
            StalkerBootstrapRecipe::AuthOnly,
            StalkerBootstrapRecipe::ModuleGated,
        ],
    };
    chain.push(default);
    chain.dedup();
    chain
}

/// Map a `StalkerBootstrapRecipe` to its runtime `StalkerRecipeSpec`. The mapping is the
/// canonical place where the per-recipe knobs (handshake-extra, token-in-query, etc.)
/// live — we keep it as a single match so a new recipe is a one-line change.
pub fn recipe_spec_for(recipe: StalkerBootstrapRecipe) -> StalkerRecipeSpec {
    match recipe {
        StalkerBootstrapRecipe::GenericSafe | StalkerBootstrapRecipe::AuthOnly => StalkerRecipeSpec {
            recipe,
            emit_handshake_extra: false,
            require_portal_handshake: false,
            token_in_query: false,
        },
        StalkerBootstrapRecipe::LegacyMag => StalkerRecipeSpec {
            recipe,
            emit_handshake_extra: false,
            require_portal_handshake: true,
            token_in_query: true,
        },
        StalkerBootstrapRecipe::ModuleGated| StalkerBootstrapRecipe::StrictMag | StalkerBootstrapRecipe::AuthStrictMag => StalkerRecipeSpec {
            recipe,
            emit_handshake_extra: true,
            require_portal_handshake: true,
            token_in_query: true,
        },
        StalkerBootstrapRecipe::PortalPreferred => StalkerRecipeSpec {
            recipe,
            emit_handshake_extra: false,
            require_portal_handshake: true,
            token_in_query: false,
        },
        StalkerBootstrapRecipe::LocalizationStrict => StalkerRecipeSpec {
            recipe,
            emit_handshake_extra: true,
            require_portal_handshake: false,
            token_in_query: false,
        },
    }
}

/// Inspect the handshake evidence (`js.*` keys emitted by the portal) and classify the
/// observed portal flavour. The result is currently diagnostic/runtime metadata; the
/// recipe chain itself remains explicit so the client does not pretend to auto-adapt to
/// fingerprints it does not actually consume.
pub fn detect_fingerprint(
    handshake_status: u16,
    handshake_body_keys: &[String],
    preset: StalkerMagPreset,
) -> StalkerPortalFingerprint {
    let spec = stalker_mag_preset_spec(preset);
    let body = handshake_body_keys.iter().map(String::as_str).collect::<Vec<_>>().join("|");
    if body.contains("js.token") {
        if (400..500).contains(&handshake_status) {
            if spec.emit_token_query {
                StalkerPortalFingerprint::AuthStrictMag
            } else {
                StalkerPortalFingerprint::AuthOnly
            }
        } else {
            StalkerPortalFingerprint::AuthOnly
        }
    } else if body.contains("js.id") && !body.contains("js.token") {
        StalkerPortalFingerprint::StrictMag
    } else if body.contains("js.mac") {
        StalkerPortalFingerprint::BasicMac
    } else if body.contains("js.module") {
        StalkerPortalFingerprint::ModuleGated
    } else {
        StalkerPortalFingerprint::BasicMac
    }
}

/// Reorder the load-URL candidates according to the user's `endpoint_preference`.
/// `Auto` is a no-op (preserve the recipe's default order). `ServerLoad` moves
/// `server/load.php` to the front. `Portal` moves `portal.php` to the front.
/// The relative order of other candidates is preserved.
pub fn apply_endpoint_preference(
    pref: StalkerEndpointPreference,
    mut candidates: Vec<crate::utils::network::stalker::url_factory::StalkerLoadUrl>,
) -> Vec<crate::utils::network::stalker::url_factory::StalkerLoadUrl> {
    match pref {
        StalkerEndpointPreference::Auto => candidates,
        StalkerEndpointPreference::ServerLoad => {
            rotate_to_front(&mut candidates, "server/load.php");
            candidates
        }
        StalkerEndpointPreference::Portal => {
            rotate_to_front(&mut candidates, "portal.php");
            candidates
        }
    }
}

fn rotate_to_front(candidates: &mut Vec<crate::utils::network::stalker::url_factory::StalkerLoadUrl>, path: &str) {
    if let Some(pos) = candidates.iter().position(|c| c.load_url.ends_with(path)) {
        if pos == 0 {
            return;
        }
        let item = candidates.remove(pos);
        candidates.insert(0, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_for_auto_ends_with_default_recipe() {
        let chain = fallback_recipes_for(StalkerAuthMode::Auto, StalkerMagPreset::GenericSafe);
        let default = default_recipe_for(StalkerAuthMode::Auto, StalkerMagPreset::GenericSafe).recipe;
        assert_eq!(*chain.last().unwrap(), default);
    }

    #[test]
    fn fallback_for_credentials_includes_auth_only() {
        let chain = fallback_recipes_for(StalkerAuthMode::CredentialsOnly, StalkerMagPreset::GenericSafe);
        assert!(chain.contains(&StalkerBootstrapRecipe::AuthOnly));
    }

    #[test]
    fn fallback_for_mac_plus_credentials_includes_auth_strict() {
        let chain = fallback_recipes_for(StalkerAuthMode::MacPlusCredentials, StalkerMagPreset::Mag254Strict);
        assert!(chain.contains(&StalkerBootstrapRecipe::AuthStrictMag));
    }

    #[test]
    fn detect_fingerprint_maps_token_response() {
        let f = detect_fingerprint(200, &["js.token".to_string()], StalkerMagPreset::GenericSafe);
        assert!(matches!(f, StalkerPortalFingerprint::AuthOnly));
    }

    #[test]
    fn detect_fingerprint_maps_id_only_response_to_strict() {
        let f = detect_fingerprint(200, &["js.id".to_string()], StalkerMagPreset::GenericSafe);
        assert!(matches!(f, StalkerPortalFingerprint::StrictMag));
    }

    #[test]
    fn detect_fingerprint_defaults_to_basic_mac() {
        let f = detect_fingerprint(200, &[], StalkerMagPreset::GenericSafe);
        assert!(matches!(f, StalkerPortalFingerprint::BasicMac));
    }

    #[test]
    fn detect_fingerprint_module_gated() {
        let f = detect_fingerprint(200, &["js.module".to_string()], StalkerMagPreset::GenericSafe);
        assert!(matches!(f, StalkerPortalFingerprint::ModuleGated));
    }

    #[test]
    fn recipe_spec_for_strict_mag_enables_extras() {
        let spec = recipe_spec_for(StalkerBootstrapRecipe::StrictMag);
        assert!(spec.emit_handshake_extra);
        assert!(spec.require_portal_handshake);
        assert!(spec.token_in_query);
    }

    fn make_candidates() -> Vec<crate::utils::network::stalker::url_factory::StalkerLoadUrl> {
        vec![
            crate::utils::network::stalker::url_factory::StalkerLoadUrl {
                load_url: "http://portal.example/server/load.php".to_string(),
                referer: "http://portal.example/c/".to_string(),
            },
            crate::utils::network::stalker::url_factory::StalkerLoadUrl {
                load_url: "http://portal.example/portal.php".to_string(),
                referer: "http://portal.example/c/".to_string(),
            },
            crate::utils::network::stalker::url_factory::StalkerLoadUrl {
                load_url: "http://portal.example/c/".to_string(),
                referer: "http://portal.example/c/".to_string(),
            },
        ]
    }

    #[test]
    fn endpoint_preference_auto_preserves_order() {
        let cands = make_candidates();
        let rotated = apply_endpoint_preference(StalkerEndpointPreference::Auto, cands.clone());
        let urls: Vec<&str> = rotated.iter().map(|c| c.load_url.as_str()).collect();
        assert_eq!(urls, vec![
            "http://portal.example/server/load.php",
            "http://portal.example/portal.php",
            "http://portal.example/c/",
        ]);
    }

    #[test]
    fn endpoint_preference_server_load_moves_first() {
        let cands = make_candidates();
        let rotated = apply_endpoint_preference(StalkerEndpointPreference::ServerLoad, cands);
        assert!(rotated[0].load_url.ends_with("server/load.php"));
    }

    #[test]
    fn endpoint_preference_portal_moves_first() {
        let cands = make_candidates();
        let rotated = apply_endpoint_preference(StalkerEndpointPreference::Portal, cands);
        assert!(rotated[0].load_url.ends_with("portal.php"));
    }

    #[test]
    fn endpoint_preference_missing_path_is_noop() {
        // No `server/load.php` in the list — rotation must not panic and must keep
        // the original order.
        let cands = vec![crate::utils::network::stalker::url_factory::StalkerLoadUrl {
            load_url: "http://portal.example/portal.php".to_string(),
            referer: "http://portal.example/c/".to_string(),
        }];
        let rotated = apply_endpoint_preference(StalkerEndpointPreference::ServerLoad, cands);
        assert_eq!(rotated.len(), 1);
        assert!(rotated[0].load_url.ends_with("portal.php"));
    }
}
