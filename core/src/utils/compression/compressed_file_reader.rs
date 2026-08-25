use crate::utils::{
    compression::compression_utils::{is_gzip, is_zlib_header},
    file_reader, open_readonly_file,
};
use flate2::bufread::{GzDecoder, ZlibDecoder};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

pub struct CompressedFileReader {
    reader: BufReader<Box<dyn Read>>,
}

impl CompressedFileReader {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let file = open_readonly_file(path)?;

        let mut buffered_file = file_reader(file);
        let mut header = [0u8; 2];
        buffered_file.read_exact(&mut header)?;
        buffered_file.seek(SeekFrom::Start(0))?;

        let reader: Box<dyn Read> = if is_gzip(&header) {
            Box::new(GzDecoder::new(buffered_file))
        } else if is_zlib_header(&header) {
            Box::new(ZlibDecoder::new(buffered_file))
        } else {
            Box::new(buffered_file)
        };

        Ok(Self { reader: file_reader(reader) })
    }
}

// Implement the Read trait for CompressedFileReader
impl Read for CompressedFileReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.reader.read(buf) }
}

// Implement BufRead for CompressedFileReader
impl BufRead for CompressedFileReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> { self.reader.fill_buf() }

    fn consume(&mut self, amt: usize) { self.reader.consume(amt); }
}

impl Iterator for CompressedFileReader {
    type Item = std::io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None, // EOF
            Ok(_) => Some(Ok(line.trim_end().to_string())),
            Err(e) => Some(Err(e)),
        }
    }
}
