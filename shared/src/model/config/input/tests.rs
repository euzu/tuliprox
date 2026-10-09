use super::*;
use crate::model::{ClusterFlags, ConfigInputUpdateQualityDto, Prepare, StalkerAuthMode, StalkerDeviceProfileDto};
use std::{collections::HashSet, net::IpAddr};

fn create_test_dto() -> ConfigInputDto { ConfigInputDto { name: "test_input".intern(), ..ConfigInputDto::default() } }

fn prepare_dto(dto: &mut ConfigInputDto) -> Result<u16, TuliproxError> { dto.prepare(0, false, &HashSet::new(), None) }

mod aliases;
mod input;
mod provider;
mod staged_media_server;
mod stalker;
