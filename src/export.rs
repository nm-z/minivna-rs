use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::model::{CalibratedSample, RawSample, ScanMode, ScanSpec};

pub fn write_csv_atomic(target: &Path, mode: ScanMode, samples: &[CalibratedSample]) -> Result<()> {
    if samples.is_empty() {
        bail!("refusing to write an empty scan");
    }
    if target.exists() {
        bail!("refusing to overwrite existing output {}", target.display());
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {}", parent.display()))?;

    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no filename: {}", target.display()))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));

    let result = write_csv_file(&temporary, mode, samples);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    fs::rename(&temporary, target).with_context(|| {
        format!(
            "failed to atomically publish {} as {}",
            temporary.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn write_csv_file(path: &Path, mode: ScanMode, samples: &[CalibratedSample]) -> Result<()> {
    let file = create_new(path)?;
    let mut writer = BufWriter::new(file);
    let loss_header = match mode {
        ScanMode::Reflection => "Return Loss(dB)",
        ScanMode::Transmission => "Transmission Loss(dB)",
    };
    writeln!(
        writer,
        "Frequency(Hz),{loss_header},Phase(deg),Rs,SWR,Xs,|Z|,Theta"
    )?;
    for sample in samples {
        let frequency = format_java_integer(sample.frequency_hz, 10);
        let loss = format_java_number(sample.loss_db, 2, 3);
        let phase = format_java_number(sample.phase_deg, 2, 3);
        let resistance = format_java_number(sample.resistance_ohm, 1, 5);
        let swr = format_java_number(sample.swr, 2, 3);
        let reactance = format_java_number(sample.reactance_ohm, 1, 5);
        let impedance = format_java_number(sample.impedance_ohm, 1, 5);
        // The pinned exporter accidentally uses getZFormat(), not its
        // dedicated theta formatter.
        let theta = format_java_number(sample.theta_deg, 1, 5);
        writeln!(
            writer,
            "{frequency},{loss},{phase},{resistance},{swr},{reactance},{impedance},{theta}"
        )?;
    }
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(|error| error.into_error())
        .context("failed to finish CSV buffer")?;
    file.sync_all().context("failed to sync completed CSV")?;
    Ok(())
}

fn format_java_integer(value: u64, maximum_integer_digits: usize) -> String {
    let value = value.to_string();
    value[value.len().saturating_sub(maximum_integer_digits)..].to_owned()
}

fn format_java_number(value: f64, fraction_digits: usize, maximum_integer_digits: usize) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        // DecimalFormat's infinity symbol is U+221E. The official exporter
        // writes Cp850, where that symbol is unmappable and OutputStreamWriter
        // replaces it with '?'.
        return if value.is_sign_negative() {
            "-?".to_owned()
        } else {
            "?".to_owned()
        };
    }

    let formatted = format!("{value:.fraction_digits$}");
    let sign_bytes = usize::from(formatted.starts_with('-'));
    let decimal = formatted[sign_bytes..]
        .find('.')
        .map(|offset| sign_bytes + offset)
        .unwrap_or(formatted.len());
    let integer_digits = decimal - sign_bytes;
    if integer_digits <= maximum_integer_digits {
        return formatted;
    }
    let trim = integer_digits - maximum_integer_digits;
    format!(
        "{}{}",
        &formatted[..sign_bytes],
        &formatted[sign_bytes + trim..]
    )
}

fn create_new(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create temporary output {}", path.display()))
}

#[derive(Serialize)]
struct JsonScan<'a> {
    format: &'static str,
    temperature_c: f64,
    supply_v: f64,
    temperature_source: &'a str,
    start_hz: u64,
    stop_hz: u64,
    points: usize,
    mode: ScanMode,
    samples: Vec<JsonSample>,
}

#[derive(Serialize)]
struct JsonSample {
    frequency_hz: u64,
    p1: u32,
    p2: u32,
    p3: u32,
    p4: u32,
    real: f64,
    imaginary: f64,
    loss_db: f64,
    phase_deg: f64,
    resistance_ohm: f64,
    swr: f64,
    reactance_ohm: f64,
    impedance_ohm: f64,
    theta_deg: f64,
}

pub fn write_scan_json_atomic(
    target: &Path,
    spec: &ScanSpec,
    temperature_c: f64,
    supply_v: f64,
    temperature_source: &str,
    raw: &[RawSample],
    calibrated: &[CalibratedSample],
) -> Result<()> {
    if target.exists() {
        bail!("refusing to overwrite existing output {}", target.display());
    }
    if raw.len() != calibrated.len() || raw.len() != spec.points {
        bail!(
            "JSON output has {} raw and {} calibrated samples; expected {}",
            raw.len(),
            calibrated.len(),
            spec.points
        );
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("raw output path has no filename"))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary: PathBuf =
        parent.join(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));
    let file = create_new(&temporary)?;
    let scan = JsonScan {
        format: "minivna-scan-v1",
        temperature_c,
        supply_v,
        temperature_source,
        start_hz: spec.start_hz,
        stop_hz: spec.stop_hz,
        points: spec.points,
        mode: spec.mode,
        samples: raw
            .iter()
            .zip(calibrated)
            .map(|(raw, calibrated)| JsonSample {
                frequency_hz: raw.frequency_hz,
                p1: raw.p1,
                p2: raw.p2,
                p3: raw.p3,
                p4: raw.p4,
                real: raw.real,
                imaginary: raw.imaginary,
                loss_db: calibrated.loss_db,
                phase_deg: calibrated.phase_deg,
                resistance_ohm: calibrated.resistance_ohm,
                swr: calibrated.swr,
                reactance_ohm: calibrated.reactance_ohm,
                impedance_ohm: calibrated.impedance_ohm,
                theta_deg: calibrated.theta_deg,
            })
            .collect(),
    };
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &scan)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

pub fn write_bytes_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    if target.exists() {
        bail!("refusing to overwrite existing output {}", target.display());
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no filename"))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        let mut file = create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{Calibration, DEFAULT_CALIBRATION_FILENAME};

    fn bundled_calibration_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("calibrations")
            .join(DEFAULT_CALIBRATION_FILENAME)
    }

    #[test]
    fn writes_legacy_compatible_header_and_precision() {
        let directory = std::env::temp_dir().join(format!(
            "minivna-export-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = directory.join("scan.csv");
        let sample = CalibratedSample {
            frequency_hz: 45_000_000,
            loss_db: -0.567,
            phase_deg: 13.019,
            resistance_ohm: 118.24,
            swr: 30.381,
            reactance_ohm: 404.06,
            impedance_ohm: 420.99,
            theta_deg: 73.67,
        };
        write_csv_atomic(&output, ScanMode::Reflection, &[sample]).unwrap();
        let text = fs::read_to_string(&output).unwrap();
        assert_eq!(
            text,
            "Frequency(Hz),Return Loss(dB),Phase(deg),Rs,SWR,Xs,|Z|,Theta\n\
             45000000,-0.57,13.02,118.2,30.38,404.1,421.0,73.7\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn csv_bytes_match_pinned_vnaj_replay_oracle() {
        let directory = std::env::temp_dir().join(format!(
            "minivna-vnaj-csv-oracle-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = directory.join("scan.csv");
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
        let calibration = Calibration::load(&bundled_calibration_path()).unwrap();
        let calibrated = calibration.calibrate(&spec, &raw, 54.0).unwrap();

        write_csv_atomic(&output, ScanMode::Reflection, &calibrated).unwrap();
        assert_eq!(
            fs::read(&output).unwrap(),
            include_bytes!("../tests/fixtures/vnaj-oracle-reflection-11.csv")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn csv_number_format_matches_vnaj_edge_conventions() {
        assert_eq!(format_java_number(f64::INFINITY, 2, 3), "?");
        assert_eq!(format_java_number(f64::NEG_INFINITY, 2, 3), "-?");
        assert_eq!(format_java_number(f64::NAN, 2, 3), "NaN");
        assert_eq!(format_java_number(123_456.74, 1, 5), "23456.7");
        assert_eq!(format_java_integer(12_345_678_901, 10), "2345678901");
    }

    #[test]
    fn json_output_contains_temperature_raw_and_calibrated_values() {
        let directory = std::env::temp_dir().join(format!(
            "minivna-json-export-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = directory.join("scan.json");
        let spec = ScanSpec {
            start_hz: 45_000_000,
            stop_hz: 45_000_010,
            points: 2,
            mode: ScanMode::Reflection,
        };
        let raw = vec![
            RawSample {
                frequency_hz: 45_000_000,
                real: 500.0,
                imaginary: 500.0,
                p1: 10_000,
                p2: 9_000,
                p3: 20_000,
                p4: 19_000,
            },
            RawSample {
                frequency_hz: 45_000_010,
                real: 501.0,
                imaginary: 502.0,
                p1: 10_001,
                p2: 8_999,
                p3: 20_002,
                p4: 18_998,
            },
        ];
        let calibrated = raw
            .iter()
            .map(|sample| CalibratedSample {
                frequency_hz: sample.frequency_hz,
                loss_db: -20.0,
                phase_deg: 10.0,
                resistance_ohm: 50.0,
                swr: 1.2,
                reactance_ohm: 2.0,
                impedance_ohm: 50.04,
                theta_deg: 2.29,
            })
            .collect::<Vec<_>>();

        write_scan_json_atomic(
            &output,
            &spec,
            48.9,
            4.5,
            "device query after acquisition",
            &raw,
            &calibrated,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(value["temperature_c"], 48.9);
        assert_eq!(value["supply_v"], 4.5);
        assert_eq!(value["samples"][0]["p1"], 10_000);
        assert_eq!(value["samples"][0]["loss_db"], -20.0);
        fs::remove_dir_all(directory).unwrap();
    }
}
