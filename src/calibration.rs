use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use num_complex::Complex64;
use serde::{Deserialize, Serialize};

use crate::model::{CalibratedSample, RawSample, ScanMode, ScanSpec};

const CALIBRATION_FORMAT: &str = "minivna-rs-calibration-v1";
pub const DEFAULT_CALIBRATION_FILENAME: &str = "NATES-miniVNA_Tiny.json";
pub const NEUTRAL_CALIBRATION_TEMPERATURE_C: f64 = 40.0;
pub const DEFAULT_CALIBRATION_BYTES: &[u8] =
    include_bytes!("../calibrations/NATES-miniVNA_Tiny.json");
const TEMPERATURE_REFERENCE_C: f64 = NEUTRAL_CALIBRATION_TEMPERATURE_C;
const TEMPERATURE_CORRECTION: f64 = 0.011;
const GAIN_CORRECTION: f64 = 1.0;
const PHASE_CORRECTION_DEG: f64 = 0.0;
const IF_PHASE_CORRECTION_DEG_PER_C: f64 = 1.10;
const REFERENCE_RESISTANCE_OHM: f64 = 50.0;
const MIN_LOSS_DB: f64 = -120.0;
const SMALL_PHASE_DEG: f64 = 0.1;
const RADIANS_TO_DEGREES: f64 = 57.295_779_513_082_32;
const SWITCH_POINTS_HZ: [u64; 2] = [1_045_000_000, 1_525_000_000];

#[derive(Clone, Debug)]
pub struct Calibration {
    source: PathBuf,
    analyser_type: String,
    mode: ScanMode,
    start_hz: u64,
    stop_hz: u64,
    temperature_c: f64,
    points: Vec<CalibrationPoint>,
}

#[derive(Clone, Debug)]
struct CalibrationPoint {
    frequency_hz: u64,
    e00: Complex64,
    e11: Complex64,
    delta_e: Complex64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CalibrationFile {
    format: String,
    analyser_type: String,
    comment: Option<String>,
    start_hz: u64,
    stop_hz: u64,
    points: usize,
    overscans: usize,
    mode: ScanMode,
    load: Option<CalibrationBlock>,
    open: Option<CalibrationBlock>,
    short: Option<CalibrationBlock>,
    loopback: Option<CalibrationBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CalibrationBlock {
    device_temperature_c: Option<f64>,
    start_hz: u64,
    stop_hz: u64,
    points: usize,
    samples: Vec<CalibrationSample>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CalibrationSample {
    frequency_hz: u64,
    real: f64,
    imaginary: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationStandard {
    Open,
    Short,
    Load,
    Loopback,
}

impl CalibrationStandard {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Short => "short",
            Self::Load => "load",
            Self::Loopback => "loopback",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CalibrationCapture {
    pub standard: CalibrationStandard,
    pub device_temperature_c: f64,
    pub samples: Vec<RawSample>,
}

pub fn build_native_calibration(
    spec: &ScanSpec,
    captures: &[CalibrationCapture],
    comment: Option<String>,
) -> Result<Vec<u8>> {
    spec.validate()?;

    let mut load = None;
    let mut open = None;
    let mut short = None;
    let mut loopback = None;
    for capture in captures {
        if !capture.device_temperature_c.is_finite() {
            bail!(
                "{} calibration temperature must be finite",
                capture.standard.name()
            );
        }
        if capture.samples.len() != spec.points {
            bail!(
                "{} calibration captured {} points, expected {}",
                capture.standard.name(),
                capture.samples.len(),
                spec.points
            );
        }
        for (index, sample) in capture.samples.iter().enumerate() {
            let expected_frequency = spec.frequency_at(index);
            if sample.frequency_hz != expected_frequency {
                bail!(
                    "{} calibration point {} has frequency {}, expected {}",
                    capture.standard.name(),
                    index + 1,
                    sample.frequency_hz,
                    expected_frequency
                );
            }
        }
        let block = CalibrationBlock {
            device_temperature_c: Some(capture.device_temperature_c),
            start_hz: spec.start_hz,
            stop_hz: spec.stop_hz,
            points: spec.points,
            samples: capture
                .samples
                .iter()
                .map(|sample| CalibrationSample {
                    frequency_hz: sample.frequency_hz,
                    real: sample.real,
                    imaginary: sample.imaginary,
                })
                .collect(),
        };
        let slot = match capture.standard {
            CalibrationStandard::Open => &mut open,
            CalibrationStandard::Short => &mut short,
            CalibrationStandard::Load => &mut load,
            CalibrationStandard::Loopback => &mut loopback,
        };
        if slot.replace(block).is_some() {
            bail!(
                "calibration contains duplicate {} captures",
                capture.standard.name()
            );
        }
    }

    match spec.mode {
        ScanMode::Reflection => {
            if open.is_none() || short.is_none() || load.is_none() {
                bail!("reflection calibration requires open, short, and load captures");
            }
            if loopback.is_some() {
                bail!("reflection calibration does not use a loopback capture");
            }
        }
        ScanMode::Transmission => {
            if open.is_none() || loopback.is_none() {
                bail!("transmission calibration requires open and loopback captures");
            }
            if short.is_some() || load.is_some() {
                bail!("transmission calibration does not use short or load captures");
            }
        }
    }

    let file = CalibrationFile {
        format: CALIBRATION_FORMAT.to_owned(),
        analyser_type: "20".to_owned(),
        comment,
        start_hz: spec.start_hz,
        stop_hz: spec.stop_hz,
        points: spec.points,
        overscans: 0,
        mode: spec.mode,
        load,
        open,
        short,
        loopback,
    };
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    Calibration::from_bytes(&bytes, PathBuf::from("<generated calibration>"))?;
    Ok(bytes)
}

impl Calibration {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read calibration {}", path.display()))?;
        Self::from_bytes(&bytes, path.to_path_buf())
    }

    fn from_bytes(bytes: &[u8], source: PathBuf) -> Result<Self> {
        let mut file: CalibrationFile = serde_json::from_slice(bytes).with_context(|| {
            format!(
                "{} is not a native minivna calibration JSON file; import legacy .cal files with scripts/import-legacy-calibration",
                source.display()
            )
        })?;
        if file.format != CALIBRATION_FORMAT {
            bail!(
                "unsupported calibration format {:?}; expected {CALIBRATION_FORMAT:?}",
                file.format
            );
        }
        if file.start_hz >= file.stop_hz {
            bail!("calibration has an invalid frequency range");
        }

        for block in [
            &mut file.load,
            &mut file.open,
            &mut file.short,
            &mut file.loopback,
        ]
        .into_iter()
        .flatten()
        {
            validate_block(block)?;
            suppress_switch_points(block);
        }

        let temperature_c = [
            file.load.as_ref(),
            file.open.as_ref(),
            file.short.as_ref(),
            file.loopback.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(|block| block.device_temperature_c)
        .fold((0.0, 0_usize), |(sum, count), value| {
            (sum + value, count + 1)
        });
        let temperature_c = match temperature_c {
            (sum, count) if count > 0 => sum / count as f64,
            _ => bail!("calibration contains no device temperature"),
        };

        let points = match file.mode {
            ScanMode::Reflection => {
                let open = required_block(file.open.as_ref(), "open")?;
                let short = required_block(file.short.as_ref(), "short")?;
                let load = required_block(file.load.as_ref(), "load")?;
                ensure_matching_blocks(&[open, short, load])?;
                open.samples
                    .iter()
                    .zip(&short.samples)
                    .zip(&load.samples)
                    .map(|((open, short), load)| reflection_point(open, short, load, temperature_c))
                    .collect::<Result<Vec<_>>>()?
            }
            ScanMode::Transmission => {
                let open = required_block(file.open.as_ref(), "open")?;
                let loopback = required_block(file.loopback.as_ref(), "loopback")?;
                ensure_matching_blocks(&[open, loopback])?;
                open.samples
                    .iter()
                    .zip(&loopback.samples)
                    .map(|(open, loopback)| transmission_point(open, loopback, temperature_c))
                    .collect()
            }
        };
        if points.len() != file.points {
            bail!(
                "calibration header says {} points but contains {}",
                file.points,
                points.len()
            );
        }
        if !points
            .windows(2)
            .all(|pair| pair[0].frequency_hz < pair[1].frequency_hz)
        {
            bail!("calibration frequencies are not strictly increasing");
        }

        Ok(Self {
            source,
            analyser_type: file.analyser_type,
            mode: file.mode,
            start_hz: file.start_hz,
            stop_hz: file.stop_hz,
            temperature_c,
            points,
        })
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn analyser_type(&self) -> &str {
        &self.analyser_type
    }

    pub fn mode(&self) -> ScanMode {
        self.mode
    }

    pub fn start_hz(&self) -> u64 {
        self.start_hz
    }

    pub fn stop_hz(&self) -> u64 {
        self.stop_hz
    }

    pub fn temperature_c(&self) -> f64 {
        self.temperature_c
    }

    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    pub fn calibrate(
        &self,
        spec: &ScanSpec,
        raw: &[RawSample],
        measurement_temperature_c: f64,
    ) -> Result<Vec<CalibratedSample>> {
        self.validate_scan(spec, measurement_temperature_c)?;
        if raw.len() != spec.points {
            bail!(
                "raw result has {} samples, expected {}",
                raw.len(),
                spec.points
            );
        }

        raw.iter()
            .enumerate()
            .map(|(index, sample)| {
                self.calibrate_sample_unchecked(spec, index, sample, measurement_temperature_c)
            })
            .collect()
    }

    pub fn calibrate_sample(
        &self,
        spec: &ScanSpec,
        index: usize,
        sample: &RawSample,
        measurement_temperature_c: f64,
    ) -> Result<CalibratedSample> {
        self.validate_scan(spec, measurement_temperature_c)?;
        if index >= spec.points {
            bail!(
                "raw point index {} is outside the configured {} points",
                index,
                spec.points
            );
        }
        self.calibrate_sample_unchecked(spec, index, sample, measurement_temperature_c)
    }

    fn validate_scan(&self, spec: &ScanSpec, measurement_temperature_c: f64) -> Result<()> {
        if spec.mode != self.mode {
            bail!(
                "scan mode {} does not match {} calibration",
                spec.mode,
                self.mode
            );
        }
        if spec.start_hz < self.start_hz || spec.stop_hz > self.stop_hz {
            bail!(
                "scan {}..{} Hz is outside calibration range {}..{} Hz",
                spec.start_hz,
                spec.stop_hz,
                self.start_hz,
                self.stop_hz
            );
        }
        if !measurement_temperature_c.is_finite() {
            bail!("measurement temperature must be finite");
        }
        Ok(())
    }

    fn calibrate_sample_unchecked(
        &self,
        spec: &ScanSpec,
        index: usize,
        sample: &RawSample,
        measurement_temperature_c: f64,
    ) -> Result<CalibratedSample> {
        // This intentionally differs from ScanSpec::frequency_at. The pinned
        // vna/J headless oracle resizes calibration points with `/ points`,
        // while it labels received samples with `/ (points - 1)`.
        let calibration_step = (spec.stop_hz - spec.start_hz) / spec.points as u64;
        let calibration_frequency = spec.start_hz + calibration_step * index as u64;
        let point = self.interpolate_point(calibration_frequency)?;
        let corrected = corrected_complex(sample.real, sample.imaginary, measurement_temperature_c);
        match self.mode {
            ScanMode::Reflection => Ok(calibrate_reflection(
                sample.frequency_hz,
                corrected,
                &point,
                self.temperature_c,
                measurement_temperature_c,
            )),
            ScanMode::Transmission => Ok(calibrate_transmission(
                sample.frequency_hz,
                corrected,
                &point,
                self.temperature_c,
                measurement_temperature_c,
            )),
        }
    }

    fn interpolate_point(&self, frequency_hz: u64) -> Result<CalibrationPoint> {
        let index = self
            .points
            .partition_point(|point| point.frequency_hz < frequency_hz);
        if index < self.points.len() && self.points[index].frequency_hz == frequency_hz {
            return Ok(self.points[index].clone());
        }
        let (lower, upper) = match index {
            0 => bail!(
                "frequency {frequency_hz} Hz is below first calibration point {} Hz",
                self.points[0].frequency_hz
            ),
            index if index >= self.points.len() => (
                &self.points[self.points.len() - 2],
                &self.points[self.points.len() - 1],
            ),
            index => (&self.points[index - 1], &self.points[index]),
        };
        let width = (upper.frequency_hz - lower.frequency_hz) as f64;
        let fraction = (frequency_hz as f64 - lower.frequency_hz as f64) / width;
        Ok(CalibrationPoint {
            frequency_hz,
            e00: lerp_complex(lower.e00, upper.e00, fraction),
            e11: lerp_complex(lower.e11, upper.e11, fraction),
            delta_e: lerp_complex(lower.delta_e, upper.delta_e, fraction),
        })
    }
}

fn required_block<'a>(
    block: Option<&'a CalibrationBlock>,
    name: &str,
) -> Result<&'a CalibrationBlock> {
    block.ok_or_else(|| anyhow::anyhow!("calibration is missing its {name} block"))
}

fn validate_block(block: &CalibrationBlock) -> Result<()> {
    if block.samples.len() != block.points {
        bail!(
            "calibration block says {} points but contains {}",
            block.points,
            block.samples.len()
        );
    }
    if block.samples.len() < 2 {
        bail!("calibration block needs at least two samples");
    }
    Ok(())
}

fn ensure_matching_blocks(blocks: &[&CalibrationBlock]) -> Result<()> {
    let first = blocks[0];
    for block in &blocks[1..] {
        if block.samples.len() != first.samples.len() {
            bail!("calibration standard blocks have different point counts");
        }
        for (left, right) in first.samples.iter().zip(&block.samples) {
            if left.frequency_hz != right.frequency_hz {
                bail!("calibration standard blocks have different frequency grids");
            }
        }
    }
    Ok(())
}

fn suppress_switch_points(block: &mut CalibrationBlock) {
    for switch in SWITCH_POINTS_HZ {
        for index in 2..block.samples.len().saturating_sub(1) {
            if block.samples[index - 1].frequency_hz <= switch
                && block.samples[index].frequency_hz > switch
            {
                let previous = block.samples[index - 1].clone();
                block.samples[index].real = previous.real;
                block.samples[index].imaginary = previous.imaginary;
                break;
            }
        }
    }
}

fn corrected_complex(real: f64, imaginary: f64, temperature_c: f64) -> Complex64 {
    let factor = 1.0 - ((TEMPERATURE_REFERENCE_C - temperature_c) * TEMPERATURE_CORRECTION);
    let real = real * factor;
    let imaginary = imaginary * factor;
    let phase_radians = PHASE_CORRECTION_DEG.to_radians();
    let corrected_real = real.trunc();
    let corrected_imaginary =
        ((imaginary * GAIN_CORRECTION - real * phase_radians.sin()) / phase_radians.cos()).trunc();
    Complex64::new(corrected_real, corrected_imaginary)
}

fn reflection_point(
    open: &CalibrationSample,
    short: &CalibrationSample,
    load: &CalibrationSample,
    temperature_c: f64,
) -> Result<CalibrationPoint> {
    let m1 = corrected_complex(open.real, open.imaginary, temperature_c);
    let m2 = corrected_complex(short.real, short.imaginary, temperature_c);
    let m3 = corrected_complex(load.real, load.imaginary, temperature_c);

    let p1 = (-m2 - m1) * (m1 - m3);
    let p2 = (-m1) * (m2 - m1);
    let p3 = (-m2 - m1) * -1.0;
    let p4 = (-m1) * -2.0;
    let denominator = p3 - p4;
    if denominator.norm_sqr() == 0.0 {
        bail!(
            "singular reflection calibration at {} Hz",
            open.frequency_hz
        );
    }
    let delta_e = complex_divide(p1 + p2, denominator);
    let e11_denominator = -m2 - m1;
    if e11_denominator.norm_sqr() == 0.0 {
        bail!(
            "singular reflection tracking term at {} Hz",
            open.frequency_hz
        );
    }
    let e11 = complex_divide(m2 - m1 - 2.0 * delta_e, e11_denominator);
    let e00 = m1 - m1 * e11 + delta_e;
    Ok(CalibrationPoint {
        frequency_hz: open.frequency_hz,
        e00,
        e11,
        delta_e,
    })
}

fn transmission_point(
    open: &CalibrationSample,
    loopback: &CalibrationSample,
    temperature_c: f64,
) -> CalibrationPoint {
    let open = corrected_complex(open.real, open.imaginary, temperature_c);
    let loopback_corrected = corrected_complex(loopback.real, loopback.imaginary, temperature_c);
    let e00 = Complex64::new(
        (loopback_corrected.im - 512.0) * 0.003,
        (loopback_corrected.re - 512.0) * 0.003,
    );
    let e11 = Complex64::new((open.im - 512.0) * 0.003, (open.re - 512.0) * 0.003);
    CalibrationPoint {
        frequency_hz: loopback.frequency_hz,
        e00,
        e11,
        delta_e: e00 - e11,
    }
}

fn calibrate_reflection(
    frequency_hz: u64,
    measured: Complex64,
    point: &CalibrationPoint,
    calibration_temperature_c: f64,
    measurement_temperature_c: f64,
) -> CalibratedSample {
    let rho = complex_divide(measured - point.e00, measured * point.e11 - point.delta_e);
    let magnitude = complex_abs(rho).min(1.0);
    let swr = (1.0 + magnitude) / (1.0 - magnitude);
    let loss_db = (20.0 * magnitude.log10()).max(MIN_LOSS_DB);

    let mut phase_deg = rho.arg() * RADIANS_TO_DEGREES;
    if (0.0..SMALL_PHASE_DEG).contains(&phase_deg) {
        phase_deg = SMALL_PHASE_DEG;
    } else if (-SMALL_PHASE_DEG..0.0).contains(&phase_deg) {
        phase_deg = -SMALL_PHASE_DEG;
    }
    phase_deg +=
        (calibration_temperature_c - measurement_temperature_c) * IF_PHASE_CORRECTION_DEG_PER_C;
    phase_deg = fold_phase(phase_deg);

    let phase_radians = phase_deg / RADIANS_TO_DEGREES;
    let reflected_real = phase_radians.cos() * magnitude;
    let reflected_imaginary = phase_radians.sin() * magnitude;
    let denominator =
        (1.0 - reflected_real) * (1.0 - reflected_real) + reflected_imaginary * reflected_imaginary;
    let reactance_ohm = 2.0 * reflected_imaginary / denominator * REFERENCE_RESISTANCE_OHM;
    let resistance_ohm =
        ((1.0 - reflected_real * reflected_real - reflected_imaginary * reflected_imaginary)
            / denominator
            * REFERENCE_RESISTANCE_OHM)
            .max(0.0);
    let impedance_ohm = (resistance_ohm * resistance_ohm + reactance_ohm * reactance_ohm).sqrt();
    let theta_deg =
        (std::f64::consts::FRAC_PI_2 - resistance_ohm.atan2(reactance_ohm)) * RADIANS_TO_DEGREES;

    CalibratedSample {
        frequency_hz,
        loss_db,
        phase_deg,
        resistance_ohm,
        swr,
        reactance_ohm,
        impedance_ohm,
        theta_deg,
    }
}

fn calibrate_transmission(
    frequency_hz: u64,
    corrected: Complex64,
    point: &CalibrationPoint,
    calibration_temperature_c: f64,
    measurement_temperature_c: f64,
) -> CalibratedSample {
    let measured = Complex64::new(
        (corrected.im - 512.0) * 0.003,
        (corrected.re - 512.0) * 0.003,
    );
    let gain = complex_divide(measured - point.e11, point.delta_e);
    let magnitude = complex_abs(gain);
    let loss_db = (20.0 * magnitude.log10()).max(MIN_LOSS_DB);
    let mut phase_deg = -gain.arg() * RADIANS_TO_DEGREES
        + (calibration_temperature_c - measurement_temperature_c) * IF_PHASE_CORRECTION_DEG_PER_C;
    // This is the pinned vna/J Tiny behavior, including its asymmetric
    // single-step phase adjustment.
    if phase_deg > 180.0 {
        phase_deg -= 180.0;
    } else if phase_deg < -180.0 {
        phase_deg += -180.0;
    }

    let inverse_magnitude = 10_f64.powf(-loss_db / 20.0);
    let doubled_reference = 2.0 * REFERENCE_RESISTANCE_OHM;
    let tangent = (phase_deg / RADIANS_TO_DEGREES).tan();
    let resistance_ohm = doubled_reference * inverse_magnitude / (1.0 + tangent.powf(2.0)).sqrt()
        - doubled_reference;
    let reactance_ohm = (-resistance_ohm + doubled_reference) * tangent;
    let impedance_ohm = (resistance_ohm * resistance_ohm + reactance_ohm * reactance_ohm).sqrt();

    CalibratedSample {
        frequency_hz,
        loss_db,
        phase_deg,
        resistance_ohm,
        swr: 0.0,
        reactance_ohm,
        impedance_ohm,
        theta_deg: 0.0,
    }
}

fn fold_phase(mut phase: f64) -> f64 {
    while phase > 180.0 {
        phase -= 360.0;
    }
    while phase < -180.0 {
        phase += 360.0;
    }
    phase
}

fn lerp_complex(left: Complex64, right: Complex64, fraction: f64) -> Complex64 {
    left + (right - left) * fraction
}

/// Apache Commons Math 3.6.1 `Complex.divide`, reproduced operation-for-
/// operation so vna/J replay results do not depend on num-complex's division
/// implementation.
fn complex_divide(numerator: Complex64, denominator: Complex64) -> Complex64 {
    let c = denominator.re;
    let d = denominator.im;
    if c == 0.0 && d == 0.0 {
        return Complex64::new(f64::NAN, f64::NAN);
    }
    if c.abs() < d.abs() {
        let q = c / d;
        let divisor = c * q + d;
        Complex64::new(
            (numerator.re * q + numerator.im) / divisor,
            (numerator.im * q - numerator.re) / divisor,
        )
    } else {
        let q = d / c;
        let divisor = d * q + c;
        Complex64::new(
            (numerator.im * q + numerator.re) / divisor,
            (numerator.im - numerator.re * q) / divisor,
        )
    }
}

/// Apache Commons Math 3.6.1 `Complex.abs`.
fn complex_abs(value: Complex64) -> f64 {
    if value.re.is_nan() || value.im.is_nan() {
        return f64::NAN;
    }
    if value.re.is_infinite() || value.im.is_infinite() {
        return f64::INFINITY;
    }
    if value.re.abs() < value.im.abs() {
        if value.im == 0.0 {
            value.re.abs()
        } else {
            let q = value.re / value.im;
            value.im.abs() * (1.0 + q * q).sqrt()
        }
    } else if value.re == 0.0 {
        value.im.abs()
    } else {
        let q = value.im / value.re;
        value.re.abs() * (1.0 + q * q).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrected_samples_truncate_like_java_casts() {
        let value = corrected_complex(100.9, -100.9, 40.0);
        assert_eq!(value, Complex64::new(100.0, -100.0));
    }

    #[test]
    fn phase_folding_handles_multiple_turns() {
        assert_eq!(fold_phase(541.0), -179.0);
        assert_eq!(fold_phase(-541.0), 179.0);
    }

    #[test]
    fn captured_standards_build_a_loadable_native_calibration() {
        let original: CalibrationFile = serde_json::from_slice(DEFAULT_CALIBRATION_BYTES).unwrap();
        let spec = ScanSpec {
            start_hz: original.start_hz,
            stop_hz: original.stop_hz,
            points: original.points,
            mode: original.mode,
        };
        let capture = |standard, block: &CalibrationBlock| CalibrationCapture {
            standard,
            device_temperature_c: block.device_temperature_c.unwrap(),
            samples: block
                .samples
                .iter()
                .enumerate()
                .map(|(index, sample)| RawSample {
                    frequency_hz: spec.frequency_at(index),
                    real: sample.real,
                    imaginary: sample.imaginary,
                    p1: 0,
                    p2: 0,
                    p3: 0,
                    p4: 0,
                })
                .collect(),
        };
        let captures = vec![
            capture(CalibrationStandard::Open, original.open.as_ref().unwrap()),
            capture(CalibrationStandard::Short, original.short.as_ref().unwrap()),
            capture(CalibrationStandard::Load, original.load.as_ref().unwrap()),
        ];

        let bytes = build_native_calibration(&spec, &captures, Some("test calibration".to_owned()))
            .unwrap();
        let generated: CalibrationFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(generated.format, CALIBRATION_FORMAT);
        assert_eq!(generated.points, spec.points);
        assert!(generated.open.is_some());
        assert!(generated.short.is_some());
        assert!(generated.load.is_some());
        assert!(generated.loopback.is_none());
    }

    #[test]
    fn reflection_math_matches_pinned_headless_oracle_replay() {
        // Oracle:
        // vnaJ-hl.3.3.3_jp.jar
        // SHA-256 8a02ee6f9680e2c5c92a270c0ad00943a38e525de435a65a50d72b23186cec60
        // NATES-miniVNA_Tiny.cal
        // SHA-256 9e286165944eb62aea412073e0450e779638bdf7c7ed926d6dbbc40706d72f5e
        let spec = ScanSpec {
            start_hz: 45_000_000,
            stop_hz: 45_015_000,
            points: 11,
            mode: ScanMode::Reflection,
        };
        let raw_values = [
            (-704_651.0, -2_666_882.0),
            (-706_611.5, -2_666_029.5),
            (-706_526.0, -2_665_773.5),
            (-707_293.0, -2_666_200.0),
            (-707_037.5, -2_666_967.0),
            (-706_526.0, -2_665_518.0),
            (-706_440.5, -2_665_774.0),
            (-706_952.0, -2_667_478.5),
            (-707_293.0, -2_665_688.5),
            (-707_208.0, -2_666_285.5),
            (-709_765.0, -2_667_222.5),
        ];
        let expected_phase: [f64; 11] = [
            -53.364_059_739_312_665,
            -53.409_040_403_973_24,
            -53.407_926_003_406_99,
            -53.418_433_260_311_79,
            -53.405_544_836_995_034,
            -53.405_538_759_806_18,
            -53.400_230_019_492_04,
            -53.395_178_917_337_86,
            -53.415_150_167_381_97,
            -53.407_084_643_441_87,
            -53.449_491_257_979_61,
        ];
        let expected_reactance: [f64; 11] = [
            -99.491_659_180_268_34,
            -99.394_398_683_382_36,
            -99.396_806_488_305_57,
            -99.374_107_914_725_76,
            -99.401_951_614_822_37,
            -99.401_964_746_706_13,
            -99.413_437_187_505_31,
            -99.424_354_822_257_14,
            -99.381_199_412_451_68,
            -99.398_624_415_774_45,
            -99.307_062_412_838_53,
        ];
        let raw = raw_values
            .into_iter()
            .enumerate()
            .map(|(index, (real, imaginary))| RawSample {
                frequency_hz: spec.frequency_at(index),
                real,
                imaginary,
                p1: 0,
                p2: 0,
                p3: 0,
                p4: 0,
            })
            .collect::<Vec<_>>();
        let calibration = Calibration::from_bytes(
            DEFAULT_CALIBRATION_BYTES,
            PathBuf::from(DEFAULT_CALIBRATION_FILENAME),
        )
        .unwrap();
        let first_point = calibration.interpolate_point(45_000_000).unwrap();
        assert_eq!(first_point.e00.re.to_bits(), 4_684_922_377_502_839_519);
        assert_eq!(first_point.e00.im.to_bits(), 13_907_811_478_741_522_421);
        assert_eq!(first_point.e11.re.to_bits(), 4_585_602_666_524_452_289);
        assert_eq!(first_point.e11.im.to_bits(), 13_809_654_074_250_507_207);
        assert_eq!(first_point.delta_e.re.to_bits(), 13_923_796_872_665_287_521);
        assert_eq!(first_point.delta_e.im.to_bits(), 4_702_006_069_701_303_795);
        let actual = calibration.calibrate(&spec, &raw, 54.0).unwrap();
        for (index, raw_sample) in raw.iter().enumerate() {
            let live = calibration
                .calibrate_sample(&spec, index, raw_sample, 54.0)
                .unwrap();
            let batch = &actual[index];
            assert_eq!(
                (
                    live.frequency_hz,
                    live.loss_db.to_bits(),
                    live.phase_deg.to_bits(),
                    live.resistance_ohm.to_bits(),
                    live.swr.to_bits(),
                    live.reactance_ohm.to_bits(),
                    live.impedance_ohm.to_bits(),
                    live.theta_deg.to_bits(),
                ),
                (
                    batch.frequency_hz,
                    batch.loss_db.to_bits(),
                    batch.phase_deg.to_bits(),
                    batch.resistance_ohm.to_bits(),
                    batch.swr.to_bits(),
                    batch.reactance_ohm.to_bits(),
                    batch.impedance_ohm.to_bits(),
                    batch.theta_deg.to_bits(),
                ),
                "live and batch calibration differ at replay point {index}"
            );
        }
        let expected_resistance_bits = [
            0,
            0,
            0,
            0,
            0,
            0,
            4_395_219_820_811_917_287,
            0,
            4_395_215_311_092_477_454,
            0,
            0,
        ];

        for (index, sample) in actual.iter().enumerate() {
            assert_eq!(sample.frequency_hz, spec.frequency_at(index));
            assert_eq!(
                sample.loss_db.to_bits(),
                0.0_f64.to_bits(),
                "loss mismatch at replay point {index}"
            );
            assert_eq!(
                sample.phase_deg.to_bits(),
                expected_phase[index].to_bits(),
                "phase mismatch at replay point {index}"
            );
            assert_eq!(
                sample.resistance_ohm.to_bits(),
                expected_resistance_bits[index],
                "resistance mismatch at replay point {index}"
            );
            assert_eq!(
                sample.swr.to_bits(),
                f64::INFINITY.to_bits(),
                "SWR mismatch at replay point {index}"
            );
            assert_eq!(
                sample.reactance_ohm.to_bits(),
                expected_reactance[index].to_bits(),
                "reactance mismatch at replay point {index}"
            );
            assert_eq!(
                sample.impedance_ohm.to_bits(),
                expected_reactance[index].abs().to_bits(),
                "impedance mismatch at replay point {index}"
            );
            assert_eq!(
                sample.theta_deg.to_bits(),
                (-90.0_f64).to_bits(),
                "theta mismatch at replay point {index}"
            );
        }
    }

    #[test]
    fn two_point_calibration_grid_uses_span_divided_by_points_like_vnaj() {
        let spec = ScanSpec {
            start_hz: 45_000_000,
            stop_hz: 60_000_000,
            points: 2,
            mode: ScanMode::Reflection,
        };
        let calibration = Calibration::from_bytes(
            DEFAULT_CALIBRATION_BYTES,
            PathBuf::from(DEFAULT_CALIBRATION_FILENAME),
        )
        .unwrap();
        let raw = [
            RawSample {
                frequency_hz: 45_000_000,
                real: -704_651.0,
                imaginary: -2_666_882.0,
                p1: 0,
                p2: 0,
                p3: 0,
                p4: 0,
            },
            RawSample {
                frequency_hz: 60_000_000,
                real: -706_611.5,
                imaginary: -2_666_029.5,
                p1: 0,
                p2: 0,
                p3: 0,
                p4: 0,
            },
        ];
        let result = calibration.calibrate(&spec, &raw, 54.0).unwrap();
        assert_eq!(result[0].frequency_hz, 45_000_000);
        assert_eq!(result[1].frequency_hz, 60_000_000);
        assert_eq!(
            result[1].phase_deg.to_bits(),
            (-45.301_690_161_734_37_f64).to_bits()
        );
        assert_eq!(
            result[1].reactance_ohm.to_bits(),
            (-119.817_478_440_780_3_f64).to_bits()
        );

        let oracle_second_calibration_point = calibration.interpolate_point(52_500_000).unwrap();
        let endpoint_calibration_point = calibration.interpolate_point(60_000_000).unwrap();
        assert_ne!(
            oracle_second_calibration_point.e00.re.to_bits(),
            endpoint_calibration_point.e00.re.to_bits()
        );
    }
}
