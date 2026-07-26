use std::fmt;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const BYTES_PER_SAMPLE: usize = 12;
pub const MIN_FREQUENCY_HZ: u64 = 1_000_000;
pub const MAX_FREQUENCY_HZ: u64 = 3_000_000_000;
pub const FREQUENCY_COMMAND_DIVISOR: u64 = 10;
pub const MIN_SCAN_SPAN_HZ: u64 = 1_000;
pub const MAX_VERIFIED_SCAN_POINTS: usize = 30_001;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Csv,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Csv => f.write_str("csv"),
            Self::Json => f.write_str("json"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    #[default]
    Reflection,
    Transmission,
}

impl ScanMode {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Reflection => "7",
            Self::Transmission => "6",
        }
    }
}

impl fmt::Display for ScanMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reflection => f.write_str("reflection"),
            Self::Transmission => f.write_str("transmission"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScanSpec {
    pub start_hz: u64,
    pub stop_hz: u64,
    pub points: usize,
    pub mode: ScanMode,
}

impl ScanSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.stop_hz <= self.start_hz {
            return Err(SpecError::InvalidRange {
                start: self.start_hz,
                stop: self.stop_hz,
            });
        }
        let span = self.stop_hz - self.start_hz;
        if span < MIN_SCAN_SPAN_HZ {
            return Err(SpecError::RangeTooNarrow {
                start: self.start_hz,
                stop: self.stop_hz,
            });
        }
        if self.start_hz < MIN_FREQUENCY_HZ {
            return Err(SpecError::FrequencyTooLow(self.start_hz));
        }
        if self.stop_hz > MAX_FREQUENCY_HZ {
            return Err(SpecError::FrequencyTooHigh(self.stop_hz));
        }
        if self.points < 2 {
            return Err(SpecError::TooFewPoints(self.points));
        }
        let available = max_supported_points(self.start_hz, self.stop_hz);
        if self.points > available {
            return Err(SpecError::TooManyPointsForRange {
                requested: self.points,
                available,
            });
        }
        self.expected_bytes()?;
        Ok(())
    }

    pub fn expected_bytes(&self) -> Result<usize, SpecError> {
        self.points
            .checked_mul(BYTES_PER_SAMPLE)
            .ok_or(SpecError::TooManyPoints(self.points))
    }

    /// Reproduces the frequency labels assigned by the miniVNA Tiny driver in
    /// the pinned vna/J headless oracle. The device response contains no
    /// frequency field; vna/J calculates one integer step and accumulates it.
    pub fn frequency_at(&self, index: usize) -> u64 {
        let step = (self.stop_hz - self.start_hz) / (self.points - 1) as u64;
        self.start_hz + step * index as u64
    }
}

pub fn max_supported_points(start_hz: u64, stop_hz: u64) -> usize {
    let start_tick = start_hz / FREQUENCY_COMMAND_DIVISOR;
    let stop_tick = stop_hz / FREQUENCY_COMMAND_DIVISOR;
    let distinct_ticks = stop_tick.saturating_sub(start_tick).saturating_add(1);
    usize::try_from(distinct_ticks)
        .unwrap_or(usize::MAX)
        .min(MAX_VERIFIED_SCAN_POINTS)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawSample {
    pub frequency_hz: u64,
    pub real: f64,
    pub imaginary: f64,
    pub p1: u32,
    pub p2: u32,
    pub p3: u32,
    pub p4: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CalibratedSample {
    pub frequency_hz: u64,
    pub loss_db: f64,
    pub phase_deg: f64,
    pub resistance_ohm: f64,
    pub swr: f64,
    pub reactance_ohm: f64,
    pub impedance_ohm: f64,
    pub theta_deg: f64,
}

#[derive(Debug, Error)]
pub enum SpecError {
    #[error(
        "start frequency {0} Hz is outside the valid start range {MIN_FREQUENCY_HZ}..={max_start} Hz",
        max_start = MAX_FREQUENCY_HZ - MIN_SCAN_SPAN_HZ
    )]
    FrequencyTooLow(u64),
    #[error(
        "stop frequency {0} Hz is outside the valid stop range {min_stop}..={MAX_FREQUENCY_HZ} Hz",
        min_stop = MIN_FREQUENCY_HZ + MIN_SCAN_SPAN_HZ
    )]
    FrequencyTooHigh(u64),
    #[error(
        "stop frequency ({stop} Hz) must be at least {MIN_SCAN_SPAN_HZ} Hz above start frequency ({start} Hz); valid device frequencies are {MIN_FREQUENCY_HZ}..={MAX_FREQUENCY_HZ} Hz"
    )]
    InvalidRange { start: u64, stop: u64 },
    #[error(
        "scan range {start}..{stop} Hz is narrower than the minimum span of {MIN_SCAN_SPAN_HZ} Hz; valid device frequencies are {MIN_FREQUENCY_HZ}..={MAX_FREQUENCY_HZ} Hz"
    )]
    RangeTooNarrow { start: u64, stop: u64 },
    #[error("a scan needs at least two points, got {0}")]
    TooFewPoints(usize),
    #[error("point count {0} is too large")]
    TooManyPoints(usize),
    #[error(
        "requested {requested} points, but valid points for this range are \"max\" or an integer from 2 through {available}"
    )]
    TooManyPointsForRange { requested: usize, available: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_mapping_matches_vnaj_integer_step_accumulation() {
        let spec = ScanSpec {
            start_hz: 45_000_000,
            stop_hz: 60_000_001,
            points: 10_001,
            mode: ScanMode::Reflection,
        };
        assert_eq!(spec.frequency_at(0), 45_000_000);
        assert_eq!(spec.frequency_at(1), 45_001_500);
        assert_eq!(spec.frequency_at(10_000), 60_000_000);
        assert!(spec.frequency_at(9_999) < spec.frequency_at(10_000));
    }

    #[test]
    fn two_point_raw_grid_uses_both_endpoints_like_vnaj() {
        let spec = ScanSpec {
            start_hz: 1_000_000,
            stop_hz: 4_000_000,
            points: 2,
            mode: ScanMode::Reflection,
        };
        assert_eq!(spec.frequency_at(0), 1_000_000);
        assert_eq!(spec.frequency_at(1), 4_000_000);
    }

    #[test]
    fn automatic_point_limit_uses_completed_hardware_limit_evidence() {
        assert_eq!(max_supported_points(45_000_000, 60_000_000), 30_001);
        assert_eq!(max_supported_points(45_000_000, 45_000_090), 10);
    }
}
