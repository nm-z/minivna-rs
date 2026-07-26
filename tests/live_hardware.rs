use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use minivna::calibration::{Calibration, DEFAULT_CALIBRATION_FILENAME};
use minivna::device::{DEFAULT_BAUD, DeviceConfig, DeviceManager, resolve_port};
use minivna::export::write_csv_atomic;
use minivna::model::{BYTES_PER_SAMPLE, RawSample, ScanMode, ScanSpec};
use minivna::protocol::decode_sample;
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, StopBits};

fn bundled_calibration_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("calibrations")
        .join(DEFAULT_CALIBRATION_FILENAME)
}

fn relay_payloads(path: &Path) -> Vec<Vec<u8>> {
    let text = fs::read_to_string(path).expect("read fresh usbmon capture");
    let mut payloads = Vec::new();
    let mut current = Vec::new();
    let mut incoming = false;
    for line in text.lines() {
        if line.starts_with("< ") {
            current.clear();
            incoming = true;
            continue;
        }
        if line.starts_with("> ") {
            incoming = false;
            continue;
        }
        if line == "--" {
            if incoming && !current.is_empty() {
                payloads.push(std::mem::take(&mut current));
            }
            incoming = false;
            continue;
        }
        if !incoming {
            continue;
        }
        for token in line.split_ascii_whitespace() {
            if token.len() != 2 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                break;
            }
            current.push(u8::from_str_radix(token, 16).expect("relay hex byte"));
        }
    }
    payloads
}

fn fresh_capture_paths() -> (PathBuf, PathBuf) {
    let relay = std::env::var_os("MINIVNA_JAVA_RELAY")
        .map(PathBuf::from)
        .expect("MINIVNA_JAVA_RELAY must name a capture made by live-parity.sh");
    let csv = std::env::var_os("MINIVNA_JAVA_CSV")
        .map(PathBuf::from)
        .expect("MINIVNA_JAVA_CSV must name the CSV from that same real scan");
    (relay, csv)
}

fn required_u64(name: &str) -> u64 {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must describe the fresh real scan"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned integer"))
}

fn point_probe_deadline(points: usize) -> Duration {
    let point_budget_ms = u64::try_from(points)
        .expect("point count fits u64")
        .saturating_mul(25);
    Duration::from_millis(point_budget_ms.saturating_add(30_000))
}

#[test]
#[ignore = "requires the physical miniVNA Tiny and a fresh vna/J usbmon capture"]
fn fresh_real_vnaj_scan_replays_to_bit_identical_rust_csv() {
    let (relay, java_csv) = fresh_capture_paths();
    let payloads = relay_payloads(&relay);
    let payload_lengths = payloads.iter().map(Vec::len).collect::<Vec<_>>();
    eprintln!(
        "fresh real relay: {} payloads, {} direct bytes, payload range {}..={} bytes",
        payload_lengths.len(),
        payload_lengths.iter().sum::<usize>(),
        payload_lengths.iter().min().copied().unwrap_or(0),
        payload_lengths.iter().max().copied().unwrap_or(0),
    );
    assert!(payloads.len() >= 3, "temperature, supply, and scan data");
    assert_eq!(payloads[0].len(), 2, "real temperature response");
    assert_eq!(payloads[1].len(), 2, "real supply response");

    let temperature_c = f64::from(u16::from_le_bytes(
        payloads[0].as_slice().try_into().expect("temperature u16"),
    )) / 10.0;
    let supply_v = f64::from(u16::from_le_bytes(
        payloads[1].as_slice().try_into().expect("supply u16"),
    )) * 6.0
        / 1024.0;
    assert!(
        (0.0..=100.0).contains(&temperature_c),
        "real device temperature {temperature_c}"
    );
    assert!(
        (0.0..=7.0).contains(&supply_v),
        "real device supply {supply_v}"
    );

    let scan_bytes = payloads[2..].concat();
    let spec = ScanSpec {
        start_hz: required_u64("MINIVNA_START_HZ"),
        stop_hz: required_u64("MINIVNA_STOP_HZ"),
        points: usize::try_from(required_u64("MINIVNA_POINTS")).expect("point count fits usize"),
        mode: ScanMode::Reflection,
    };
    let fresh_rust_scan = real_unchecked_scan(
        spec.start_hz,
        spec.stop_hz,
        spec.points,
        Duration::from_secs(180),
    );
    assert_eq!(
        fresh_rust_scan.bytes.len(),
        spec.expected_bytes().unwrap(),
        "this acceptance test must itself complete a fresh physical scan"
    );
    assert_eq!(scan_bytes.len(), spec.expected_bytes().unwrap());
    let raw = scan_bytes
        .chunks_exact(BYTES_PER_SAMPLE)
        .enumerate()
        .map(|(index, frame)| {
            decode_sample(
                frame.try_into().expect("12-byte hardware frame"),
                spec.frequency_at(index),
            )
        })
        .collect::<Vec<RawSample>>();
    assert!(raw.iter().any(|sample| sample.real.fract() != 0.0));

    let calibration = Calibration::load(&bundled_calibration_path()).unwrap();
    let calibrated = calibration.calibrate(&spec, &raw, temperature_c).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "minivna-live-replay-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let rust_csv = directory.join("rust.csv");
    write_csv_atomic(&rust_csv, spec.mode, &calibrated).unwrap();
    assert_eq!(
        fs::read(&rust_csv).unwrap(),
        fs::read(&java_csv).unwrap(),
        "the same real hardware bytes must export identically"
    );
    fs::remove_dir_all(directory).unwrap();
}

struct ProbeResult {
    bytes: Vec<u8>,
    elapsed: Duration,
}

fn assert_real_device_ready() {
    let mut manager = DeviceManager::new(DeviceConfig {
        port: "auto".to_owned(),
        baud: DEFAULT_BAUD,
        read_slice: Duration::from_millis(100),
    });
    manager
        .ensure_open()
        .expect("open real FT230X before readiness query");
    let firmware = manager
        .query_firmware(Duration::from_secs(2))
        .expect("device must answer a framed firmware query before a test scan");
    assert!(
        firmware.starts_with("FW Tiny "),
        "unexpected real firmware identity {firmware:?}"
    );
}

fn real_unchecked_scan(
    start_hz: u64,
    stop_hz: u64,
    points: usize,
    deadline: Duration,
) -> ProbeResult {
    assert_real_device_ready();
    let port_name = resolve_port("auto").expect("resolve real FT230X");
    let mut port = serialport::new(&port_name, DEFAULT_BAUD)
        .data_bits(DataBits::Eight)
        .flow_control(FlowControl::None)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .timeout(Duration::from_millis(100))
        .open()
        .expect("open real FT230X");
    port.clear(ClearBuffer::All)
        .expect("clear real FT230X buffers");

    for field in [
        b"7\r".to_vec(),
        format!("{}\r", start_hz / 10).into_bytes(),
        format!("{}\r", stop_hz / 10).into_bytes(),
        format!("{points}\r").into_bytes(),
        b"\r".to_vec(),
    ] {
        port.write_all(&field).expect("write real scan field");
        port.flush().expect("flush real scan field");
    }

    let expected = points
        .checked_mul(BYTES_PER_SAMPLE)
        .expect("probe byte count");
    let started = Instant::now();
    let mut bytes = vec![0_u8; expected];
    let mut received = 0;
    while received < expected && started.elapsed() < deadline {
        match port.read(&mut bytes[received..]) {
            Ok(0) => {}
            Ok(count) => received += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => panic!("real scan read failed: {error}"),
        }
    }
    bytes.truncate(received);
    ProbeResult {
        bytes,
        elapsed: started.elapsed(),
    }
}

#[test]
#[ignore = "requires the physical miniVNA Tiny"]
fn real_two_point_scan_uses_both_requested_endpoints() {
    let result = real_unchecked_scan(1_000_000, 4_000_000, 2, Duration::from_secs(3));
    assert_eq!(result.bytes.len(), 2 * BYTES_PER_SAMPLE);
    let spec = ScanSpec {
        start_hz: 1_000_000,
        stop_hz: 4_000_000,
        points: 2,
        mode: ScanMode::Reflection,
    };
    let first = decode_sample(
        result.bytes[0..12].try_into().unwrap(),
        spec.frequency_at(0),
    );
    let second = decode_sample(
        result.bytes[12..24].try_into().unwrap(),
        spec.frequency_at(1),
    );
    assert_eq!(first.frequency_hz, 1_000_000);
    assert_eq!(second.frequency_hz, 4_000_000);
}

fn stop_cli_daemon(socket: &Path) {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return;
    };
    let _ = stream.write_all(b"{\"request\":\"shutdown\"}\n");
    let _ = stream.flush();
    let mut response = String::new();
    let _ = BufReader::new(stream).read_line(&mut response);
}

struct CliDaemonCleanup {
    socket: PathBuf,
    armed: bool,
}

impl Drop for CliDaemonCleanup {
    fn drop(&mut self) {
        if self.armed {
            stop_cli_daemon(&self.socket);
        }
    }
}

#[test]
#[ignore = "requires the physical miniVNA Tiny and performs two real scans"]
fn real_cli_exits_after_each_scan_while_daemon_retains_the_port() {
    assert_real_device_ready();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "minivna-real-daemon-lifecycle-{}-{nonce}",
        std::process::id()
    ));
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hardware-fast.toml");
    let executable = env!("CARGO_BIN_EXE_minivna");

    let run_scan = |debug: bool| {
        let mut command = Command::new(executable);
        command.arg("--config").arg(&config);
        if debug {
            command.arg("--debug");
        }
        command
            .arg("scan")
            .current_dir(&root)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .expect("run real minivna CLI scan")
    };

    let first = run_scan(false);
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first.status.success(),
        "first CLI scan failed:\n{}",
        first_stderr
    );
    assert!(
        first_stderr.contains("Opened "),
        "first scan must open the physical port"
    );
    assert!(
        first_stderr.contains("Opened /dev/tty"),
        "normal output must name the resolved tty device node"
    );
    assert!(
        !first_stderr.contains("Opened /dev/serial/by-id/"),
        "normal output must not expose the long stable symlink"
    );
    assert!(first_stderr.contains("% RL:"));
    assert!(first_stderr.contains("dB Phase:"));
    assert!(first_stderr.contains("Scanning:\n\t2 points\n\t45000000 to 45001000 Hz"));
    assert!(!first_stderr.contains("scan-20"));
    assert!(!first_stderr.contains("Raw point"));
    assert!(!first_stderr.contains("p1="));

    let sockets = fs::read_dir(runtime.join("minivna"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sock")
        })
        .collect::<Vec<_>>();
    assert_eq!(sockets.len(), 1, "one daemon must own this config");
    let mut cleanup = CliDaemonCleanup {
        socket: sockets[0].clone(),
        armed: true,
    };
    assert!(
        UnixStream::connect(&cleanup.socket).is_ok(),
        "the CLI exited but its detached daemon must remain reachable"
    );

    let second = run_scan(true);
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "second CLI scan failed:\n{}",
        second_stderr
    );
    assert!(
        !second_stderr.contains("Opened "),
        "the second physical scan must reuse the daemon's still-open port"
    );
    for field in [
        "\"event\":\"scan_progress\"",
        "\"event\":\"scan_sample\"",
        "\"id\":\"scan-",
        "\"timestamp\":",
        "\"frequency_hz\":",
        "\"p1\":",
        "\"p2\":",
        "\"p3\":",
        "\"p4\":",
        "\"real\":",
        "\"imaginary\":",
        "\"loss_db\":",
        "\"phase_deg\":",
        "\"resistance_ohm\":",
        "\"reactance_ohm\":",
        "\"swr\":",
        "\"impedance_ohm\":",
        "\"theta_deg\":",
    ] {
        assert!(
            second_stderr.contains(field),
            "debug scan omitted {field}:\n{second_stderr}"
        );
    }
    let output_directories = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("TEST_"))
        })
        .count();
    assert_eq!(output_directories, 2, "both real scans must publish output");

    stop_cli_daemon(&cleanup.socket);
    for _ in 0..100 {
        if !cleanup.socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !cleanup.socket.exists(),
        "daemon shutdown must remove its socket"
    );
    cleanup.armed = false;
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires the physical miniVNA Tiny and performs out-of-spec probes"]
fn real_frequency_boundary_probes_match_observed_tiny_behavior() {
    for (name, start_hz, stop_hz) in [
        ("below_minimum", 999_000, 1_000_000),
        ("at_minimum", 1_000_000, 1_001_000),
        ("at_maximum", 2_999_999_000, 3_000_000_000),
        ("above_maximum", 3_000_000_000, 3_000_001_000),
        ("wider_than_claimed", 999_000, 3_000_001_000),
    ] {
        let result = real_unchecked_scan(start_hz, stop_hz, 2, Duration::from_secs(3));
        eprintln!(
            "{name}: {start_hz}..{stop_hz} returned {}/{} bytes in {} ms",
            result.bytes.len(),
            2 * BYTES_PER_SAMPLE,
            result.elapsed.as_millis()
        );
        assert!(
            result.bytes.len() == 2 * BYTES_PER_SAMPLE,
            "{name} did not return two complete real frames"
        );
    }
}

#[test]
#[ignore = "requires the physical miniVNA Tiny and performs a long 30001-point scan"]
fn real_device_is_probed_above_the_claimed_30000_point_limit() {
    let points = 30_001;
    let result = real_unchecked_scan(45_000_000, 60_000_000, points, point_probe_deadline(points));
    eprintln!(
        "{points} points returned {}/{} bytes in {} ms",
        result.bytes.len(),
        points * BYTES_PER_SAMPLE,
        result.elapsed.as_millis()
    );
    assert_eq!(
        result.bytes.len(),
        points * BYTES_PER_SAMPLE,
        "the real device did not complete the above-limit scan"
    );
}

#[test]
#[ignore = "requires the physical miniVNA Tiny and MINIVNA_PROBE_POINTS"]
fn real_configured_point_count_probe_completes() {
    let points = usize::try_from(required_u64("MINIVNA_PROBE_POINTS"))
        .expect("MINIVNA_PROBE_POINTS fits usize");
    let start_hz = std::env::var("MINIVNA_PROBE_START_HZ")
        .map(|value| value.parse().expect("MINIVNA_PROBE_START_HZ is an integer"))
        .unwrap_or(45_000_000);
    let stop_hz = std::env::var("MINIVNA_PROBE_STOP_HZ")
        .map(|value| value.parse().expect("MINIVNA_PROBE_STOP_HZ is an integer"))
        .unwrap_or(60_000_000);
    let result = real_unchecked_scan(start_hz, stop_hz, points, point_probe_deadline(points));
    eprintln!(
        "{points} points over {start_hz}..{stop_hz} Hz returned {}/{} bytes in {} ms",
        result.bytes.len(),
        points * BYTES_PER_SAMPLE,
        result.elapsed.as_millis()
    );
    assert_eq!(
        result.bytes.len(),
        points * BYTES_PER_SAMPLE,
        "the real device did not complete the configured point-count probe"
    );
}
