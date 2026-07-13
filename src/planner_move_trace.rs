use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct PlannerMoveTraceLogger {
    writer: Option<BufWriter<File>>,
}

impl PlannerMoveTraceLogger {
    pub fn new(path: Option<&str>) -> Result<Self> {
        let writer = match path {
            Some(path) => Some(BufWriter::new(File::create(Path::new(path)).with_context(
                || format!("failed to create planner move trace log at {path}"),
            )?)),
            None => None,
        };
        Ok(Self { writer })
    }

    pub fn enabled(&self) -> bool {
        self.writer.is_some()
    }

    pub fn write_line(&mut self, line: &str) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.write_all(line.as_bytes())?;
            writer.flush()?;
        }
        Ok(())
    }
}
