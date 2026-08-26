use crate::utils::{deobscure_authenticated_bytes, obscure_authenticated_bytes};
use shared::{error::TuliproxError, model::XtreamCluster};

const TOKEN_DOMAIN: &[u8] = b"provider-resolve";
const KIND_PLAYLIST_ITEM: u8 = 1;

pub const PROVIDER_RESOLVE_ROUTE_PREFIX: &str = "/provider/resolve";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResolvePlaylistItemToken {
    pub username: String,
    pub target_id: u16,
    pub virtual_id: u32,
    pub cluster: XtreamCluster,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderResolveToken {
    PlaylistItem(ProviderResolvePlaylistItemToken),
}

fn cluster_to_u8(cluster: XtreamCluster) -> u8 {
    match cluster {
        XtreamCluster::Live => 0,
        XtreamCluster::Video => 1,
        XtreamCluster::Series => 2,
    }
}

fn cluster_from_u8(raw: u8) -> Result<XtreamCluster, TuliproxError> {
    match raw {
        0 => Ok(XtreamCluster::Live),
        1 => Ok(XtreamCluster::Video),
        2 => Ok(XtreamCluster::Series),
        _ => Err(TuliproxError::Crypto("Invalid provider resolve token cluster".to_string())),
    }
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], TuliproxError> {
    if input.len() < len {
        return Err(TuliproxError::Crypto("Provider resolve token payload is truncated".to_string()));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

pub fn encode_provider_resolve_playlist_item_token(
    secret: &[u8; 16],
    token: &ProviderResolvePlaylistItemToken,
) -> Result<String, TuliproxError> {
    let username_bytes = token.username.as_bytes();
    let username_len = u8::try_from(username_bytes.len()).map_err(|_| {
        TuliproxError::Crypto("Provider resolve token username is too long for compact encoding".to_string())
    })?;

    let mut payload = Vec::with_capacity(1 + 1 + 2 + 4 + 1 + username_bytes.len());
    payload.push(KIND_PLAYLIST_ITEM);
    payload.push(cluster_to_u8(token.cluster));
    payload.extend_from_slice(&token.target_id.to_be_bytes());
    payload.extend_from_slice(&token.virtual_id.to_be_bytes());
    payload.push(username_len);
    payload.extend_from_slice(username_bytes);
    obscure_authenticated_bytes(secret, TOKEN_DOMAIN, &payload)
}

pub fn decode_provider_resolve_token(secret: &[u8; 16], encoded: &str) -> Result<ProviderResolveToken, TuliproxError> {
    let payload = deobscure_authenticated_bytes(secret, TOKEN_DOMAIN, encoded)?;
    let mut input = payload.as_slice();
    let kind = take(&mut input, 1)?[0];
    match kind {
        KIND_PLAYLIST_ITEM => {
            let cluster = cluster_from_u8(take(&mut input, 1)?[0])?;
            let target_id = u16::from_be_bytes(
                take(&mut input, 2)?
                    .try_into()
                    .map_err(|_| TuliproxError::Crypto("Invalid provider resolve target id".to_string()))?,
            );
            let virtual_id = u32::from_be_bytes(
                take(&mut input, 4)?
                    .try_into()
                    .map_err(|_| TuliproxError::Crypto("Invalid provider resolve virtual id".to_string()))?,
            );
            let username_len = usize::from(take(&mut input, 1)?[0]);
            let username = std::str::from_utf8(take(&mut input, username_len)?)
                .map_err(|_| TuliproxError::Crypto("Invalid provider resolve username".to_string()))?
                .to_owned();
            if !input.is_empty() {
                return Err(TuliproxError::Crypto("Provider resolve token has trailing payload bytes".to_string()));
            }
            Ok(ProviderResolveToken::PlaylistItem(ProviderResolvePlaylistItemToken {
                username,
                target_id,
                virtual_id,
                cluster,
            }))
        }
        _ => Err(TuliproxError::Crypto("Unsupported provider resolve token kind".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_provider_resolve_token, encode_provider_resolve_playlist_item_token, ProviderResolvePlaylistItemToken,
        ProviderResolveToken,
    };
    use shared::model::XtreamCluster;

    #[test]
    fn playlist_item_token_roundtrip_is_compact() {
        let secret = [7u8; 16];
        let payload = ProviderResolvePlaylistItemToken {
            username: "alice".to_string(),
            target_id: 42,
            virtual_id: 81_356,
            cluster: XtreamCluster::Video,
        };

        let encoded = encode_provider_resolve_playlist_item_token(&secret, &payload).unwrap();
        let decoded = decode_provider_resolve_token(&secret, &encoded).unwrap();

        assert!(encoded.len() < 100, "token too long: {}", encoded.len());
        assert!(!encoded.contains("alice"));
        assert_eq!(decoded, ProviderResolveToken::PlaylistItem(payload));
    }

    #[test]
    fn playlist_item_token_rejects_usernames_that_do_not_fit_the_compact_format() {
        let secret = [7u8; 16];
        let payload = ProviderResolvePlaylistItemToken {
            username: "a".repeat(256),
            target_id: 42,
            virtual_id: 81_356,
            cluster: XtreamCluster::Video,
        };

        let encoded = encode_provider_resolve_playlist_item_token(&secret, &payload);

        assert!(encoded.is_err());
    }
}
