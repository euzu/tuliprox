use super::*;
use crate::{
    model::{
        ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigProviderDto, DnsPrefer, DnsScheme, InputType,
        OnConnectErrorPolicy, OnResolveErrorPolicy, Prepare, ProviderDnsDto, ProviderUrlSelectionPolicy,
    },
    utils::Internable,
};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
};

fn prepare_dto(dto: &mut ConfigInputDto) -> Result<u16, TuliproxError> { dto.prepare(0, false, &HashSet::new(), None) }

fn media_server_config_with_library() -> MediaServerInputConfigDto {
    MediaServerInputConfigDto {
        libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
        ..MediaServerInputConfigDto::default()
    }
}

mod catalog_auth;
mod input_provider_compatibility;
