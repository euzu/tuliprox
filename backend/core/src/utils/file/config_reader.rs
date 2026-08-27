//! Reading a configuration file with environment-variable resolution.
//!
//! Only the two primitives the other `utils` file readers share live here. The
//! application configuration bootstrap that used to sit alongside them - reading
//! sources, preparing batches and users, persisting config - moved to
//! `crate::config_loader`, because it reads and writes the repository and
//! `utils` must not depend on that layer.

use crate::utils::{file_reader, EnvResolvingReader};
use log::error;
use shared::utils::CONSTANTS;
use std::{
    env,
    fs::File,
    io::{self, Read},
};

pub fn config_file_reader(file: File, resolve_env: bool) -> impl Read {
    if resolve_env {
        EitherReader::Left(EnvResolvingReader::new(file_reader(file)))
    } else {
        EitherReader::Right(file_reader(file))
    }
}

pub fn resolve_env_var(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    CONSTANTS
        .re_env_var
        .replace_all(value, |caps: &regex::Captures| {
            let var_name = &caps["var"];
            env::var(var_name).unwrap_or_else(|e| {
                error!("Could not resolve env var '{var_name}': {e}");
                format!("${{env:{var_name}}}")
            })
        })
        .to_string()
}

enum EitherReader<L, R> {
    Left(L),
    Right(R),
}

impl<L: Read, R: Read> Read for EitherReader<L, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            EitherReader::Left(reader) => reader.read(buf),
            EitherReader::Right(reader) => reader.read(buf),
        }
    }
}
