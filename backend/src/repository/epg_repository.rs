use shared::error::{notify_err, TuliproxError, TuliproxErrorKind};
use crate::model::{Config, ConfigTarget, TargetOutput};
use crate::model::Epg;
use crate::repository::m3u_repository::m3u_get_epg_file_path;
use crate::repository::xtream_repository::{xtream_get_epg_file_path, xtream_get_storage_path};
use crate::utils::debug_if_enabled;
use quick_xml::Writer;
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::Path;

fn epg_write_file(target: &ConfigTarget, epg: &Epg, path: &Path) -> Result<(), TuliproxError> {
    let mut writer = Writer::new(Cursor::new(vec![]));
    match epg.write_to(&mut writer) {
        Ok(()) => {
            let result = writer.into_inner().into_inner();
            match File::create(path) {
                Ok(mut epg_file) => {
                    match epg_file.write_all("<?xml version=\"1.0\" encoding=\"utf-8\" ?><!DOCTYPE tv SYSTEM \"xmltv.dtd\">".as_bytes()) {
                        Ok(()) => {}
                        Err(err) => return Err(notify_err!(format!("failed to write epg: {} - {}", path.to_str().unwrap_or("?"), err))),
                    }
                    match epg_file.write_all(&result) {
                        Ok(()) => {
                            debug_if_enabled!("Epg for target {} written to {}", target.name, path.to_str().unwrap_or("?"));
                        }
                        Err(err) => return Err(notify_err!(format!("failed to write epg: {} - {}", path.to_str().unwrap_or("?"), err))),
                    }
                }
                Err(err) => return Err(notify_err!(format!("failed to write epg: {} - {}", path.to_str().unwrap_or("?"), err))),
            }
        }
        Err(err) => return Err(notify_err!(format!("failed to write epg: {} - {}", path.to_str().unwrap_or("?"), err))),
    }
    Ok(())
}

pub fn epg_write(cfg: &Config, target: &ConfigTarget, target_path: &Path, epg: Option<&Epg>, output: &TargetOutput) -> Result<(), TuliproxError> {
    if let Some(epg_data) = epg {
        match output {
            TargetOutput::Xtream(_) => {
                match xtream_get_storage_path(cfg, &target.name) {
                    Some(path) => {
                        let epg_path = xtream_get_epg_file_path(&path);
                        debug_if_enabled!("writing xtream epg to {}", epg_path.to_str().unwrap_or("?"));
                        epg_write_file(target, epg_data, &epg_path)?;
                    }
                    None => return Err(notify_err!(format!("failed to serialize epg for target: {}, storage path not found", target.name))),
                }
            }
            TargetOutput::M3u(_) => {
                let path = m3u_get_epg_file_path(target_path);
                debug_if_enabled!("writing m3u epg to {}", path.to_str().unwrap_or("?"));
                epg_write_file(target, epg_data, &path)?;
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    Ok(())
}
