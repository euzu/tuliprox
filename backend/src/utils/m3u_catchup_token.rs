use crate::utils::{deobscure_authenticated_bytes, obscure_authenticated_bytes};
use shared::error::TuliproxError;

const TOKEN_DOMAIN: &[u8] = b"m3u-catchup";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M3uCatchupToken {
    pub username: String,
    pub target_id: u16,
    pub virtual_id: u32,
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], TuliproxError> {
    if input.len() < len {
        return Err(TuliproxError::Crypto("M3U catchup token payload is truncated".to_string()));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

pub fn encode_m3u_catchup_token(secret: &[u8; 16], token: &M3uCatchupToken) -> Result<String, TuliproxError> {
    let username_bytes = token.username.as_bytes();
    let username_len = u8::try_from(username_bytes.len())
        .map_err(|_| TuliproxError::Crypto("M3U catchup token username is too long".to_string()))?;

    let mut payload = Vec::with_capacity(2 + 4 + 1 + username_bytes.len());
    payload.extend_from_slice(&token.target_id.to_be_bytes());
    payload.extend_from_slice(&token.virtual_id.to_be_bytes());
    payload.push(username_len);
    payload.extend_from_slice(username_bytes);
    obscure_authenticated_bytes(secret, TOKEN_DOMAIN, &payload)
}

pub fn decode_m3u_catchup_token(secret: &[u8; 16], encoded: &str) -> Result<M3uCatchupToken, TuliproxError> {
    let payload = deobscure_authenticated_bytes(secret, TOKEN_DOMAIN, encoded)?;
    let mut input = payload.as_slice();
    let target_id = u16::from_be_bytes(
        take(&mut input, 2)?
            .try_into()
            .map_err(|_| TuliproxError::Crypto("Invalid M3U catchup target id".to_string()))?,
    );
    let virtual_id = u32::from_be_bytes(
        take(&mut input, 4)?
            .try_into()
            .map_err(|_| TuliproxError::Crypto("Invalid M3U catchup virtual id".to_string()))?,
    );
    let username_len = usize::from(take(&mut input, 1)?[0]);
    let username = std::str::from_utf8(take(&mut input, username_len)?)
        .map_err(|_| TuliproxError::Crypto("Invalid M3U catchup username".to_string()))?
        .to_owned();
    if !input.is_empty() {
        return Err(TuliproxError::Crypto("M3U catchup token has trailing payload bytes".to_string()));
    }
    Ok(M3uCatchupToken { username, target_id, virtual_id })
}

#[cfg(test)]
mod tests {
    use super::{decode_m3u_catchup_token, encode_m3u_catchup_token, M3uCatchupToken};
    use crate::utils::{decode_provider_resolve_token, encode_provider_resolve_playlist_item_token, ProviderResolvePlaylistItemToken};
    use shared::model::XtreamCluster;

    #[test]
    fn m3u_catchup_token_roundtrip_is_compact() {
        let secret = [9u8; 16];
        let payload = M3uCatchupToken {
            username: "alice".to_string(),
            target_id: 42,
            virtual_id: 81_356,
        };

        let encoded = encode_m3u_catchup_token(&secret, &payload).unwrap();
        let decoded = decode_m3u_catchup_token(&secret, &encoded).unwrap();

        assert!(encoded.len() < 100, "token too long: {}", encoded.len());
        assert!(!encoded.contains("alice"));
        assert_eq!(decoded, payload);
    }

    #[test]
    fn m3u_catchup_token_rejects_overlong_usernames() {
        let secret = [9u8; 16];
        let payload = M3uCatchupToken {
            username: "a".repeat(256),
            target_id: 42,
            virtual_id: 81_356,
        };

        assert!(encode_m3u_catchup_token(&secret, &payload).is_err());
    }

    #[test]
    fn m3u_catchup_token_domain_is_distinct_from_provider_resolve() {
        let secret = [9u8; 16];
        let catchup_token = encode_m3u_catchup_token(
            &secret,
            &M3uCatchupToken {
                username: "alice".to_string(),
                target_id: 42,
                virtual_id: 81_356,
            },
        )
        .unwrap();
        let provider_token = encode_provider_resolve_playlist_item_token(
            &secret,
            &ProviderResolvePlaylistItemToken {
                username: "alice".to_string(),
                target_id: 42,
                virtual_id: 81_356,
                cluster: XtreamCluster::Video,
            },
        )
        .unwrap();

        assert!(decode_provider_resolve_token(&secret, &catchup_token).is_err());
        assert!(decode_m3u_catchup_token(&secret, &provider_token).is_err());
    }
}
