use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(ConsoleAndStderr)
        .init();
}

#[derive(Clone, Copy, Debug)]
struct ConsoleAndStderr;

struct TeeWriter {
    stderr: io::Stderr,
    console: Option<File>,
}

impl<'a> MakeWriter<'a> for ConsoleAndStderr {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            stderr: io::stderr(),
            console: OpenOptions::new().write(true).open("/dev/console").ok(),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        if let Some(console) = &mut self.console {
            console.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(console) = &mut self.console {
            console.flush()?;
        }
        Ok(())
    }
}
