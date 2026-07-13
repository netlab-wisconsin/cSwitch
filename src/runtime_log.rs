#[cfg(not(feature = "diagnostics"))]
use anyhow::Result;
#[cfg(feature = "diagnostics")]
use anyhow::{Context, Result};

#[cfg(feature = "diagnostics")]
use std::fs::File;
#[cfg(feature = "diagnostics")]
use std::io::{BufWriter, Write};
#[cfg(feature = "diagnostics")]
use std::path::Path;
#[cfg(feature = "diagnostics")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct RuntimeLogState {
    echo_stderr: bool,
    writer: Option<BufWriter<File>>,
}

#[cfg(feature = "diagnostics")]
static RUNTIME_LOG: OnceLock<Mutex<RuntimeLogState>> = OnceLock::new();

#[cfg(feature = "diagnostics")]
pub fn init(path: Option<&str>, echo_stderr: bool) -> Result<()> {
    let writer = match path {
        Some(path) => Some(BufWriter::new(
            File::create(Path::new(path))
                .with_context(|| format!("failed to create runtime log at {path}"))?,
        )),
        None => None,
    };

    let state = RUNTIME_LOG.get_or_init(|| Mutex::new(RuntimeLogState::default()));
    let mut guard = state.lock().unwrap();
    *guard = RuntimeLogState {
        echo_stderr,
        writer,
    };
    Ok(())
}

#[cfg(not(feature = "diagnostics"))]
pub fn init(_path: Option<&str>, _echo_stderr: bool) -> Result<()> {
    Ok(())
}

#[cfg(feature = "diagnostics")]
pub fn line(message: impl AsRef<str>) {
    let message = message.as_ref();

    if let Some(state) = RUNTIME_LOG.get() {
        let mut guard = state.lock().unwrap();
        if guard.echo_stderr {
            eprintln!("{message}");
        }
        if let Some(writer) = guard.writer.as_mut() {
            let _ = writer.write_all(message.as_bytes());
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
        return;
    }

    eprintln!("{message}");
}

#[cfg(not(feature = "diagnostics"))]
pub fn line(_message: impl AsRef<str>) {}
