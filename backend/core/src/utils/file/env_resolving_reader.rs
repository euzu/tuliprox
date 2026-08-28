use crate::utils::resolve_env_var;
use std::io::{self, BufRead, BufReader, Cursor, Read};

pub struct EnvResolvingReader<R: Read> {
    inner: BufReader<R>,
    buffer: Cursor<Vec<u8>>,
}

impl<R: Read> EnvResolvingReader<R> {
    pub fn new(reader: BufReader<R>) -> Self {
        Self { inner: reader, buffer: Cursor::new(Vec::new()) }
    }

    fn fill_buffer(&mut self) -> io::Result<()> {
        let mut line = String::new();
        self.buffer = Cursor::new(Vec::new());

        if self.inner.read_line(&mut line)? > 0 {
            let processed_line = resolve_env_var(&line);
            self.buffer = Cursor::new(processed_line.into_bytes());
        }

        Ok(())
    }
}

impl<R: Read> Read for EnvResolvingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.buffer.read(buf)?;

        if bytes_read == 0 {
            self.fill_buffer()?;
            self.buffer.read(buf)
        } else {
            Ok(bytes_read)
        }
    }
}

impl<R: Read> BufRead for EnvResolvingReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.fill_buffer()?;
        self.buffer.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.buffer.consume(amt);
    }
}
