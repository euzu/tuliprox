use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::{PathBuf};
use crate::utils::EnvResolvingReader;
use crate::utils::file_reader;

pub struct MultiFileReader {
    files: Vec<File>,
    current_reader: Option<EnvResolvingReader<File>>,
}

impl MultiFileReader {
    pub fn new(paths: &Vec<PathBuf>) -> io::Result<Self> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            match File::open(path) {
                Ok(file) => { files.push(file); }
                Err(err) => {
                    return Err(io::Error::new(ErrorKind::NotFound,format!("Could not find file {} {}", path.to_str().unwrap_or("?"), err)));
                }
            }
        }
        files.reverse();
        Ok(Self {
            files,
            current_reader: None,
        })
    }
}

impl Read for MultiFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.current_reader.is_none() {
                if self.files.is_empty() {
                    return Ok(0);
                }
                self.current_reader = Some(EnvResolvingReader::new(file_reader(self.files.pop().unwrap())));
                // we put a newline if the config does not have one
                if !buf.is_empty() && buf[0] != b'\n' {
                    buf[0] = b'\n';
                    return Ok(1);
                }
            }
            let reader = self.current_reader.as_mut().unwrap();
            match reader.read(buf) {
                Ok(0) => {
                    // The current reader is exhausted, move to the next one
                    self.current_reader = None;
                }
                Ok(n) => return Ok(n),
                Err(e) => return Err(e),
            }
        }
    }
}