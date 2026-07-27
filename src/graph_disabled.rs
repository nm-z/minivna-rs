use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct GraphTelemetry;

impl GraphTelemetry {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn observe(&self, _value: &Value) {}
}

pub(crate) fn run<F>(
    _telemetry: GraphTelemetry,
    _interrupted: Arc<AtomicBool>,
    _task: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    bail!("this minivna binary was built without GUI support; omit --gui")
}
