use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::model::{BYTES_PER_SAMPLE, RawSample, ScanSpec, SpecError};

#[derive(Clone, Copy, Debug)]
pub struct ScanTimeouts {
    pub idle: Duration,
    pub overall: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanProgress {
    pub received_bytes: usize,
    pub total_bytes: usize,
    pub complete_points: usize,
    pub total_points: usize,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub enum ScanObservation {
    Progress(ScanProgress),
    RawSample {
        point_index: usize,
        total_points: usize,
        sample: RawSample,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanControl {
    Continue,
    Cancel,
    Shutdown,
}

#[derive(Debug)]
pub struct ScanResult {
    pub samples: Vec<RawSample>,
    pub elapsed: Duration,
    pub received_bytes: usize,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(transparent)]
    InvalidSpec(#[from] SpecError),
    #[error("serial I/O failed after {received}/{expected} bytes: {source}")]
    Io {
        received: usize,
        expected: usize,
        #[source]
        source: io::Error,
    },
    #[error(
        "no scan data arrived for {idle_ms} ms ({received}/{expected} bytes, {elapsed_ms} ms elapsed)"
    )]
    IdleTimeout {
        idle_ms: u128,
        received: usize,
        expected: usize,
        elapsed_ms: u128,
    },
    #[error("scan exceeded its hard {limit_ms} ms deadline ({received}/{expected} bytes received)")]
    OverallTimeout {
        limit_ms: u128,
        received: usize,
        expected: usize,
    },
    #[error("scan cancelled after {received}/{expected} bytes")]
    Cancelled { received: usize, expected: usize },
    #[error("service shutdown requested after {received}/{expected} bytes")]
    Shutdown { received: usize, expected: usize },
}

/// Encodes the five CR-delimited fields accepted by the miniVNA Tiny firmware.
/// Frequencies are prescaled by ten, matching the original device driver.
pub fn encode_scan_command(spec: &ScanSpec) -> Result<Vec<u8>, SpecError> {
    Ok(encode_scan_fields(spec)?.concat())
}

fn encode_scan_fields(spec: &ScanSpec) -> Result<[Vec<u8>; 5], SpecError> {
    spec.validate()?;
    Ok([
        format!("{}\r", spec.mode.command()).into_bytes(),
        format!("{}\r", spec.start_hz / 10).into_bytes(),
        format!("{}\r", spec.stop_hz / 10).into_bytes(),
        format!("{}\r", spec.points).into_bytes(),
        b"\r".to_vec(),
    ])
}

pub fn decode_sample(frame: &[u8; BYTES_PER_SAMPLE], frequency_hz: u64) -> RawSample {
    fn u24(bytes: &[u8]) -> u32 {
        u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
    }

    let p1 = u24(&frame[0..3]);
    let p3 = u24(&frame[3..6]);
    let p2 = u24(&frame[6..9]);
    let p4 = u24(&frame[9..12]);

    RawSample {
        frequency_hz,
        real: (f64::from(p1) - f64::from(p2)) / 2.0,
        imaginary: (f64::from(p3) - f64::from(p4)) / 2.0,
        p1,
        p2,
        p3,
        p4,
    }
}

/// Performs exactly one wire-level scan.
///
/// The serial object must have a short finite read timeout. This loop keeps two
/// independent clocks: an idle deadline that moves only when actual bytes
/// arrive, and a hard overall deadline that never moves.
pub fn scan_io<T, F>(
    io: &mut T,
    spec: &ScanSpec,
    timeouts: ScanTimeouts,
    mut observe: F,
) -> Result<ScanResult, ProtocolError>
where
    T: Read + Write + ?Sized,
    F: FnMut(ScanObservation) -> ScanControl,
{
    let fields = encode_scan_fields(spec)?;
    let expected = spec.expected_bytes()?;

    // vna/J writes each protocol field independently. Keep the same write and
    // flush boundaries so live usbmon comparisons include transport behavior,
    // not just an equivalent concatenated byte string.
    for field in fields {
        io.write_all(&field).map_err(|source| ProtocolError::Io {
            received: 0,
            expected,
            source,
        })?;
        io.flush().map_err(|source| ProtocolError::Io {
            received: 0,
            expected,
            source,
        })?;
    }

    let started = Instant::now();
    let overall_deadline = started + timeouts.overall;
    let mut last_data = started;
    let mut bytes = vec![0_u8; expected];
    let mut received = 0;
    let mut samples = Vec::with_capacity(spec.points);
    let mut cancellation_requested = false;

    loop {
        let elapsed = started.elapsed();
        let progress = ScanProgress {
            received_bytes: received,
            total_bytes: expected,
            complete_points: received / BYTES_PER_SAMPLE,
            total_points: spec.points,
            elapsed,
        };
        match observe(ScanObservation::Progress(progress)) {
            ScanControl::Continue => {}
            // The firmware has no documented mid-sweep abort. Drain the exact
            // response length so a later scan cannot consume stale frames.
            ScanControl::Cancel => cancellation_requested = true,
            ScanControl::Shutdown => {
                return Err(ProtocolError::Shutdown { received, expected });
            }
        }

        if received == expected {
            if cancellation_requested {
                return Err(ProtocolError::Cancelled { received, expected });
            }
            break;
        }

        let now = Instant::now();
        if now >= overall_deadline {
            return Err(ProtocolError::OverallTimeout {
                limit_ms: timeouts.overall.as_millis(),
                received,
                expected,
            });
        }
        if now.duration_since(last_data) >= timeouts.idle {
            return Err(ProtocolError::IdleTimeout {
                idle_ms: timeouts.idle.as_millis(),
                received,
                expected,
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        let upper = (received + 8192).min(expected);
        match io.read(&mut bytes[received..upper]) {
            Ok(0) => {}
            Ok(count) => {
                received += count;
                last_data = Instant::now();
                while samples.len() < received / BYTES_PER_SAMPLE {
                    let point_index = samples.len();
                    let offset = point_index * BYTES_PER_SAMPLE;
                    let frame: &[u8; BYTES_PER_SAMPLE] = bytes[offset..offset + BYTES_PER_SAMPLE]
                        .try_into()
                        .expect("12-byte chunk");
                    let sample = decode_sample(frame, spec.frequency_at(point_index));
                    match observe(ScanObservation::RawSample {
                        point_index,
                        total_points: spec.points,
                        sample: sample.clone(),
                    }) {
                        ScanControl::Continue => {}
                        ScanControl::Cancel => cancellation_requested = true,
                        ScanControl::Shutdown => {
                            return Err(ProtocolError::Shutdown { received, expected });
                        }
                    }
                    samples.push(sample);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(source) => {
                return Err(ProtocolError::Io {
                    received,
                    expected,
                    source,
                });
            }
        }
    }

    Ok(ScanResult {
        samples,
        elapsed: started.elapsed(),
        received_bytes: received,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::model::ScanMode;

    struct ScriptedIo {
        written: Vec<u8>,
        response: Cursor<Vec<u8>>,
    }

    impl Read for ScriptedIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.response.read(buffer)
        }
    }

    impl Write for ScriptedIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn spec() -> ScanSpec {
        ScanSpec {
            start_hz: 45_000_000,
            stop_hz: 60_000_000,
            points: 2,
            mode: ScanMode::Reflection,
        }
    }

    #[test]
    fn command_matches_tiny_protocol() {
        assert_eq!(
            encode_scan_command(&spec()).unwrap(),
            b"7\r4500000\r6000000\r2\r\r"
        );
    }

    #[test]
    fn scan_decodes_two_little_endian_24_bit_samples() {
        let mut response = vec![0_u8; 24];
        response[0..3].copy_from_slice(&[0x10, 0x27, 0x00]); // p1=10000
        response[3..6].copy_from_slice(&[0x20, 0x4e, 0x00]); // p3=20000
        response[6..9].copy_from_slice(&[0x28, 0x23, 0x00]); // p2=9000
        response[9..12].copy_from_slice(&[0x38, 0x4a, 0x00]); // p4=19000
        let mut io = ScriptedIo {
            written: Vec::new(),
            response: Cursor::new(response),
        };
        let mut observed_samples = Vec::new();
        let result = scan_io(
            &mut io,
            &spec(),
            ScanTimeouts {
                idle: Duration::from_secs(1),
                overall: Duration::from_secs(1),
            },
            |observation| {
                if let ScanObservation::RawSample {
                    point_index,
                    total_points,
                    sample,
                } = observation
                {
                    observed_samples.push((point_index, total_points, sample));
                }
                ScanControl::Continue
            },
        )
        .unwrap();
        assert_eq!(io.written, b"7\r4500000\r6000000\r2\r\r");
        assert_eq!(observed_samples.len(), 2);
        assert_eq!(observed_samples[0].0, 0);
        assert_eq!(observed_samples[0].1, 2);
        assert_eq!(observed_samples[0].2.p1, 10_000);
        assert_eq!(observed_samples[0].2.p2, 9_000);
        assert_eq!(observed_samples[0].2.p3, 20_000);
        assert_eq!(observed_samples[0].2.p4, 19_000);
        assert_eq!(result.samples[0].real, 500.0);
        assert_eq!(result.samples[0].imaginary, 500.0);
        assert_eq!(result.samples[0].frequency_hz, 45_000_000);
        assert_eq!(result.samples[1].frequency_hz, 60_000_000);
    }

    #[test]
    fn odd_adc_differences_remain_half_units_like_pinned_vnaj() {
        let frame = [
            0x10, 0x27, 0x00, // p1 = 10000
            0x20, 0x4e, 0x00, // p3 = 20000
            0x29, 0x23, 0x00, // p2 = 9001
            0x39, 0x4a, 0x00, // p4 = 19001
        ];
        let sample = decode_sample(&frame, 45_000_000);
        assert_eq!(sample.real.to_bits(), 499.5_f64.to_bits());
        assert_eq!(sample.imaginary.to_bits(), 499.5_f64.to_bits());
    }

    #[test]
    fn cancellation_drains_the_full_response_before_returning() {
        let mut io = ScriptedIo {
            written: Vec::new(),
            response: Cursor::new(vec![0_u8; 24]),
        };
        let mut first_callback = true;
        let error = scan_io(
            &mut io,
            &spec(),
            ScanTimeouts {
                idle: Duration::from_secs(1),
                overall: Duration::from_secs(1),
            },
            |_| {
                if first_callback {
                    first_callback = false;
                    ScanControl::Cancel
                } else {
                    ScanControl::Continue
                }
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ProtocolError::Cancelled {
                received: 24,
                expected: 24
            }
        ));
        assert_eq!(io.response.position(), 24);
    }
}
