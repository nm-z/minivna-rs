use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Local, SecondsFormat};
use clap::{CommandFactory, Parser, Subcommand};
use minivna::calibration::{
    Calibration, CalibrationCapture, CalibrationStandard, build_native_calibration,
};
use minivna::config::{DeviceSettings, LoadedConfig, set_calibration_path, set_output_format};
use minivna::device::{DEFAULT_BAUD, DeviceConfig, DeviceManager, DirectReadings};
use minivna::export::{write_bytes_atomic, write_csv_atomic, write_scan_json_atomic};
use minivna::model::{CalibratedSample, OutputFormat, RawSample, ScanMode, ScanSpec};
use minivna::protocol::{
    ProtocolError, ScanControl, ScanObservation, ScanProgress, ScanResult, ScanTimeouts,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use signal_hook::consts::signal::{SIGINT, SIGTERM};

#[derive(Parser, Debug)]
#[command(version, about = None, disable_help_subcommand = true)]
struct Cli {
    /// Override the XDG configuration path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON Lines events on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Print every diagnostic event and every scan point.
    #[arg(long, global = true)]
    debug: bool,

    /// Select and persist the scan file format.
    #[arg(short = 'o', long = "output", global = true)]
    output: Option<OutputFormat>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one configured scan through the background port owner, then exit.
    Scan {
        #[arg(long, hide = true)]
        id: Option<String>,
    },
    /// Run a guided calibration recipe and activate the generated calibration.
    Calibrate,
    /// Restart the miniVNA controller and verify its firmware identity.
    Reset,
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
enum DaemonRequest {
    Scan {
        id: String,
        launch_directory: PathBuf,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
enum DaemonResponse {
    Event { value: Value },
    Complete { ok: bool, error: Option<String> },
}

enum EventTarget {
    Terminal,
    DaemonClient(UnixStream),
}

struct EventSink {
    format: OutputMode,
    progress_line_active: bool,
    live_metrics_expected: bool,
    style_human_output: bool,
    debug_human_output: bool,
    target: EventTarget,
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_RL_GREEN: &str = "\x1b[38;2;0;174;107m";
const ANSI_PHASE_RED: &str = "\x1b[38;2;242;40;60m";
const PATH_LABEL_WIDTH: usize = "Output directory:".len();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputMode {
    Jsonl,
    Human,
}

impl EventSink {
    fn new(json_requested: bool, debug_requested: bool) -> Self {
        Self {
            format: output_mode(json_requested),
            progress_line_active: false,
            live_metrics_expected: false,
            style_human_output: !json_requested && io::stderr().is_terminal(),
            debug_human_output: !json_requested && debug_requested,
            target: EventTarget::Terminal,
        }
    }

    fn for_daemon_client(stream: UnixStream) -> Self {
        Self {
            format: OutputMode::Jsonl,
            progress_line_active: false,
            live_metrics_expected: false,
            style_human_output: false,
            debug_human_output: false,
            target: EventTarget::DaemonClient(stream),
        }
    }

    fn emit_scan_sample(
        &mut self,
        id: &str,
        attempt: usize,
        point_index: usize,
        total_points: usize,
        raw: &RawSample,
        calibrated: &CalibratedSample,
    ) {
        let percent = (point_index + 1) as f64 * 100.0 / total_points as f64;
        self.emit(
            "scan_sample",
            Some(id),
            json!({
                "attempt": attempt,
                "point": point_index + 1,
                "total_points": total_points,
                "percent": percent,
                "frequency_hz": raw.frequency_hz,
                "p1": raw.p1,
                "p2": raw.p2,
                "p3": raw.p3,
                "p4": raw.p4,
                "real": raw.real,
                "imaginary": raw.imaginary,
                "loss_db": calibrated.loss_db,
                "phase_deg": calibrated.phase_deg,
                "resistance_ohm": json_float(calibrated.resistance_ohm),
                "swr": json_float(calibrated.swr),
                "reactance_ohm": json_float(calibrated.reactance_ohm),
                "impedance_ohm": json_float(calibrated.impedance_ohm),
                "theta_deg": json_float(calibrated.theta_deg)
            }),
        );
    }

    fn emit(&mut self, event: &str, id: Option<&str>, fields: Value) {
        let mut object = match fields {
            Value::Object(object) => object,
            other => {
                let mut object = Map::new();
                object.insert("value".to_owned(), other);
                object
            }
        };
        object.insert("event".to_owned(), Value::String(event.to_owned()));
        object.insert(
            "timestamp".to_owned(),
            Value::String(Local::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        if let Some(id) = id {
            object.insert("id".to_owned(), Value::String(id.to_owned()));
        }
        let value = Value::Object(object);
        self.emit_value(value);
    }

    fn emit_value(&mut self, value: Value) {
        if let EventTarget::DaemonClient(output) = &mut self.target {
            let response = DaemonResponse::Event { value };
            let _ = serde_json::to_writer(&mut *output, &response);
            let _ = output.write_all(b"\n");
            let _ = output.flush();
            return;
        }

        let event = value["event"].as_str().unwrap_or("invalid_event");
        if event == "scan_started" {
            self.live_metrics_expected = value["live_primary_metrics"].as_bool().unwrap_or(false);
        }
        match self.format {
            OutputMode::Jsonl => {
                let stdout = io::stdout();
                let mut output = stdout.lock();
                let _ = serde_json::to_writer(&mut output, &value);
                let _ = output.write_all(b"\n");
                let _ = output.flush();
            }
            OutputMode::Human => {
                if self.debug_human_output {
                    eprintln!("{}", debug_event_line(&value));
                    return;
                }
                if event == "scan_sample" {
                    let percent = value["percent"].as_f64().unwrap_or(0.0);
                    let loss_db = value["loss_db"].as_f64().unwrap_or(f64::NAN);
                    let phase_deg = value["phase_deg"].as_f64().unwrap_or(f64::NAN);
                    eprint!(
                        "\r{}",
                        live_measurement_line(percent, loss_db, phase_deg, self.style_human_output)
                    );
                    let _ = io::stderr().flush();
                    self.progress_line_active = true;
                } else if event == "scan_progress" {
                    if self.live_metrics_expected {
                        return;
                    }
                    let points = value["points_received"].as_u64().unwrap_or(0);
                    let total = value["total_points"].as_u64().unwrap_or(0);
                    let percent = value["percent"].as_f64().unwrap_or(0.0);
                    eprint!("\rScanning {points}/{total} points ({percent:.2}%)");
                    let _ = io::stderr().flush();
                    self.progress_line_active = true;
                } else {
                    if self.progress_line_active {
                        eprintln!();
                        self.progress_line_active = false;
                    }
                    emit_human_event(event, &value, self.style_human_output);
                }
            }
        }
    }
}

fn json_float(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else if value.is_nan() {
        Value::String("NaN".to_owned())
    } else if value.is_sign_positive() {
        Value::String("Infinity".to_owned())
    } else {
        Value::String("-Infinity".to_owned())
    }
}

fn debug_event_line(value: &Value) -> String {
    serde_json::to_string(value).expect("an in-memory JSON event is always serializable")
}

fn live_measurement_line(percent: f64, loss_db: f64, phase_deg: f64, styled: bool) -> String {
    let rl = colored_word("RL", ANSI_RL_GREEN, styled);
    let phase = colored_word("Phase", ANSI_PHASE_RED, styled);
    format!("{percent:6.2}% {rl}: {loss_db:7.2} dB {phase}: {phase_deg:7.2}°")
}

fn colored_word(word: &str, color: &str, styled: bool) -> String {
    if styled {
        format!("{color}{word}{ANSI_RESET}")
    } else {
        word.to_owned()
    }
}

fn bold_label(label: &str, styled: bool) -> String {
    if styled {
        format!("{ANSI_BOLD}{label}{ANSI_RESET}")
    } else {
        label.to_owned()
    }
}

fn aligned_path_line(label: &str, path: impl std::fmt::Display, styled: bool) -> String {
    let padding = PATH_LABEL_WIDTH.saturating_sub(label.len()) + 1;
    format!("{}{}{path}", bold_label(label, styled), " ".repeat(padding))
}

fn human_port_path(port: &str) -> String {
    fs::canonicalize(port)
        .unwrap_or_else(|_| PathBuf::from(port))
        .display()
        .to_string()
}

fn scan_summary(
    points: impl std::fmt::Display,
    start_hz: impl std::fmt::Display,
    stop_hz: impl std::fmt::Display,
) -> String {
    format!("Scanning:\n\t{points} points\n\t{start_hz} to {stop_hz} Hz")
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ReportedError {
    message: String,
    exit_code: u8,
}

fn output_mode(json_requested: bool) -> OutputMode {
    if json_requested {
        OutputMode::Jsonl
    } else {
        OutputMode::Human
    }
}

fn emit_human_event(event: &str, value: &Value, styled: bool) {
    match event {
        "config_created" => eprintln!("Created config {}", plain_value(&value["path"])),
        "config_loaded" => eprintln!("Using config {}", plain_value(&value["path"])),
        "output_format_updated" => eprintln!(
            "Output format: {} (saved in {})",
            plain_value(&value["output"]),
            plain_value(&value["config"])
        ),
        "scan_accepted" => eprintln!(
            "{}",
            aligned_path_line(
                "Output directory:",
                plain_value(&value["output_directory"]),
                styled
            )
        ),
        "calibration_loaded" => eprintln!(
            "{}",
            aligned_path_line("Calibration:", plain_value(&value["source"]), styled)
        ),
        "device_readings" => eprintln!(
            "{} {} C; {} {} V",
            bold_label("Device temperature:", styled),
            plain_value(&value["temperature_c"]),
            bold_label("supply:", styled),
            plain_value(&value["supply_v"])
        ),
        "port_opened" => eprintln!("Opened {}", human_port_path(&plain_value(&value["port"]))),
        "scan_started" => eprintln!(
            "{}",
            scan_summary(
                plain_value(&value["points"]),
                plain_value(&value["start_hz"]),
                plain_value(&value["stop_hz"])
            )
        ),
        "calibration_recipe_started" => eprintln!(
            "Calibration recipe: {} mode, {} to {} Hz, {} points per standard",
            plain_value(&value["mode"]),
            plain_value(&value["start_hz"]),
            plain_value(&value["stop_hz"]),
            plain_value(&value["points"])
        ),
        "calibration_standard_prompt" => eprintln!(
            "\nStep {}/{} — {}: {}\nPress Enter when ready, or type q then Enter to abort",
            plain_value(&value["step"]),
            plain_value(&value["total_steps"]),
            plain_value(&value["standard"]),
            plain_value(&value["instruction"])
        ),
        "calibration_standard_completed" => eprintln!(
            "Captured {}: {} points in {} ms",
            plain_value(&value["standard"]),
            plain_value(&value["points"]),
            plain_value(&value["elapsed_ms"])
        ),
        "calibration_completed" => eprintln!(
            "Calibration saved and activated: {}",
            plain_value(&value["path"])
        ),
        "scan_quality_warning" => eprintln!(
            "Warning: {} of {} points are outside the calibrated reflection circle",
            plain_value(&value["affected_points"]),
            plain_value(&value["total_points"])
        ),
        "raw_output_written" => {
            eprintln!("Raw samples: {}", plain_value(&value["path"]))
        }
        "scan_completed" => eprintln!(
            "Scan complete: {} points written to {}",
            plain_value(&value["points"]),
            plain_value(&value["output_directory"])
        ),
        "cancellation_requested" => {
            eprintln!("Cancellation requested; background hardware recovery continues")
        }
        "scan_cancelled" => eprintln!("Scan cancelled: {}", plain_value(&value["error"])),
        "scan_failed" => eprintln!("Scan failed: {}", plain_value(&value["error"])),
        _ => eprintln!("[{event}] {}", fields_for_human(value)),
    }
}

fn plain_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn fields_for_human(value: &Value) -> String {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !matches!(key.as_str(), "event" | "timestamp" | "id"))
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

struct RunOutput {
    directory: PathBuf,
    file: PathBuf,
    format: OutputFormat,
}

struct ScanRequest {
    id: String,
    spec: ScanSpec,
    calibration_path: PathBuf,
    config_path: PathBuf,
    config_snapshot: String,
    output: RunOutput,
    timeouts: ScanTimeouts,
    retries: usize,
    progress_every_points: usize,
    usb_reset_on_retry: bool,
}

const PORT_IDLE_SECONDS: u64 = 60 * 60;
const DAEMON_START_TIMEOUT_MS: u64 = 5_000;
const DAEMON_POLL_MS: u64 = 50;
const READ_SLICE_MS: u64 = 100;
const SCAN_IDLE_TIMEOUT_MS: u64 = 10_000;
const MIN_SCAN_OVERALL_TIMEOUT_MS: u64 = 300_000;
const SCAN_POINT_BUDGET_MS: u64 = 25;
const SCAN_RETRIES: usize = 2;
const PROGRESS_EVERY_POINTS: usize = 1;
const TEMPERATURE_QUERY_TIMEOUT_MS: u64 = 1_000;
const DEVICE_QUIET_WINDOW_MS: u64 = 250;

#[derive(Clone, Copy)]
struct AcquisitionPlan<'a> {
    id: &'a str,
    spec: &'a ScanSpec,
    calibration: Option<&'a Calibration>,
    timeouts: ScanTimeouts,
    retries: usize,
    progress_every_points: usize,
    usb_reset_on_retry: bool,
}

struct AcquisitionResult {
    scan: ScanResult,
    readings: DirectReadings,
    calibrated: Option<Vec<CalibratedSample>>,
}

fn main() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            if let Ok(path) = config_path_from_arguments(&arguments) {
                print_config_path(&path, json_requested_in_arguments(&arguments));
            }
            let exit_code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(u8::try_from(exit_code).unwrap_or(1));
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(reported) = error.downcast_ref::<ReportedError>() {
                ExitCode::from(reported.exit_code)
            } else {
                eprintln!("minivna: {error:#}");
                ExitCode::FAILURE
            }
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let config_path = match cli.config {
        Some(path) => path,
        None => default_config_path()?,
    };
    let config_path = absolute_config_path(config_path)?;
    print_config_path(&config_path, cli.json);
    let mut loaded = LoadedConfig::load_or_create(&config_path)?;
    let mut sink = EventSink::new(cli.json, cli.debug);
    if loaded.created {
        sink.emit(
            "config_created",
            None,
            json!({"path": loaded.path, "using_defaults": true}),
        );
    }
    if let Some(output) = cli.output {
        if loaded.settings.output != output {
            set_output_format(&loaded.path, output)?;
            loaded = LoadedConfig::load_or_create(&config_path)?;
        }
        sink.emit(
            "output_format_updated",
            None,
            json!({"output": output, "config": loaded.path}),
        );
    }
    let command = match cli.command {
        Some(command) => command,
        None => {
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&interrupted))
        .context("failed to install Ctrl-C handler")?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&interrupted))
        .context("failed to install termination handler")?;
    match command {
        Command::Scan { id } => {
            let launch_directory =
                std::env::current_dir().context("failed to determine launch cwd")?;
            run_scan_client(
                &loaded.path,
                launch_directory,
                id.unwrap_or_else(new_scan_id),
                &interrupted,
                &mut sink,
            )
        }
        Command::Calibrate => {
            stop_daemon_if_running(&loaded.path)?;
            run_calibration(loaded, &interrupted, &mut sink)
        }
        Command::Reset => {
            stop_daemon_if_running(&loaded.path)?;
            reset_controller(&loaded.settings.device, &mut sink)
        }
        Command::Daemon => run_daemon(loaded, &interrupted),
    }
}

fn default_config_path() -> Result<PathBuf> {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn absolute_config_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to determine launch cwd")?
            .join(path))
    }
}

fn config_path_from_arguments(arguments: &[OsString]) -> Result<PathBuf> {
    let mut configured = None;
    let mut arguments = arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--config" {
            configured = arguments.next().map(PathBuf::from);
            continue;
        }
        if let Some(argument) = argument.to_str()
            && let Some(path) = argument.strip_prefix("--config=")
        {
            configured = Some(PathBuf::from(path));
        }
    }
    absolute_config_path(match configured {
        Some(path) => path,
        None => default_config_path()?,
    })
}

fn json_requested_in_arguments(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .skip(1)
        .any(|argument| argument == "--json")
}

fn print_config_path(path: &Path, json_requested: bool) {
    if json_requested {
        let value = json!({
            "event": "config_path",
            "path": path,
            "timestamp": Local::now().to_rfc3339_opts(SecondsFormat::Millis, true)
        });
        let stdout = io::stdout();
        let mut output = stdout.lock();
        let _ = serde_json::to_writer(&mut output, &value);
        let _ = output.write_all(b"\n");
        let _ = output.flush();
    } else {
        eprintln!(
            "{}",
            aligned_path_line("config path:", path.display(), io::stderr().is_terminal())
        );
    }
}

fn config_path_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let xdg_config_home = xdg_config_home.filter(|value| !value.is_empty());
    let base = if let Some(path) = xdg_config_home {
        PathBuf::from(path)
    } else {
        let home = home
            .filter(|value| !value.is_empty())
            .context("HOME is not set, so the default minivna config path cannot be determined")?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("minivna").join("minivna.toml"))
}

fn run_calibration(
    loaded: LoadedConfig,
    interrupted: &AtomicBool,
    sink: &mut EventSink,
) -> Result<()> {
    let settings = &loaded.settings;
    let spec = settings.scan_spec();
    let standards = calibration_standards(spec.mode);
    sink.emit(
        "calibration_recipe_started",
        None,
        json!({
            "mode": spec.mode,
            "start_hz": spec.start_hz,
            "stop_hz": spec.stop_hz,
            "points": spec.points,
            "standards": standards.iter().map(|standard| standard.name()).collect::<Vec<_>>()
        }),
    );

    let mut manager = DeviceManager::new(to_device_config(&settings.device));
    let timeouts = automatic_scan_timeouts(spec.points);
    let mut captures = Vec::with_capacity(standards.len());

    for (index, standard) in standards.iter().copied().enumerate() {
        prompt_for_calibration_standard(
            sink,
            spec.mode,
            standard,
            index + 1,
            standards.len(),
            interrupted,
        )?;
        let id = format!("calibration-{}", standard.name());
        let plan = AcquisitionPlan {
            id: &id,
            spec: &spec,
            calibration: None,
            timeouts,
            retries: SCAN_RETRIES,
            progress_every_points: PROGRESS_EVERY_POINTS,
            usb_reset_on_retry: true,
        };
        let acquisition = acquire(&mut manager, plan, sink, |_, _| {
            if interrupted.swap(false, Ordering::Relaxed) {
                ScanControl::Shutdown
            } else {
                ScanControl::Continue
            }
        })
        .with_context(|| format!("failed to capture {} standard", standard.name()))?;
        sink.emit(
            "calibration_standard_completed",
            Some(&id),
            json!({
                "standard": standard.name(),
                "points": acquisition.scan.samples.len(),
                "elapsed_ms": acquisition.scan.elapsed.as_millis(),
                "temperature_c": acquisition.readings.temperature_c,
                "supply_v": acquisition.readings.supply_v
            }),
        );
        captures.push(CalibrationCapture {
            standard,
            device_temperature_c: acquisition.readings.temperature_c,
            samples: acquisition.scan.samples,
        });
    }
    manager.close();

    let comment = Some(format!(
        "Created by minivna calibrate at {}",
        Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    let bytes = build_native_calibration(&spec, &captures, comment)?;
    let calibration_path = allocate_calibration_output(&loaded.directory)?;
    write_bytes_atomic(&calibration_path, &bytes)?;
    Calibration::load(&calibration_path).with_context(|| {
        format!(
            "generated calibration failed validation at {}",
            calibration_path.display()
        )
    })?;

    let current = LoadedConfig::load_or_create(&loaded.path)?;
    if current.settings.scan_spec() != spec {
        bail!(
            "scan range, point count, or mode changed while calibration was running; saved {} but did not activate it",
            calibration_path.display()
        );
    }
    set_calibration_path(&loaded.path, &calibration_path)?;
    let activated = LoadedConfig::load_or_create(&loaded.path)?;
    Calibration::load(Path::new(&activated.settings.scan.calibration))
        .context("newly activated calibration could not be loaded")?;

    sink.emit(
        "calibration_completed",
        None,
        json!({
            "path": calibration_path,
            "config_calibration": activated.settings.scan.calibration,
            "config": loaded.path,
            "mode": spec.mode,
            "points": spec.points
        }),
    );
    Ok(())
}

fn calibration_standards(mode: ScanMode) -> &'static [CalibrationStandard] {
    const REFLECTION: &[CalibrationStandard] = &[
        CalibrationStandard::Open,
        CalibrationStandard::Short,
        CalibrationStandard::Load,
    ];
    const TRANSMISSION: &[CalibrationStandard] =
        &[CalibrationStandard::Open, CalibrationStandard::Loopback];
    match mode {
        ScanMode::Reflection => REFLECTION,
        ScanMode::Transmission => TRANSMISSION,
    }
}

fn calibration_standard_instruction(mode: ScanMode, standard: CalibrationStandard) -> &'static str {
    match (mode, standard) {
        (ScanMode::Reflection, CalibrationStandard::Open) => {
            "connect the OPEN standard to the DUT port"
        }
        (ScanMode::Transmission, CalibrationStandard::Open) => {
            "leave the measurement path in its OPEN/isolation condition"
        }
        (_, CalibrationStandard::Short) => "connect the SHORT standard to the DUT port",
        (_, CalibrationStandard::Load) => "connect the 50 ohm LOAD standard to the DUT port",
        (_, CalibrationStandard::Loopback) => {
            "connect the generator output to the detector input with the THRU/loopback standard"
        }
    }
}

fn prompt_for_calibration_standard(
    sink: &mut EventSink,
    mode: ScanMode,
    standard: CalibrationStandard,
    step: usize,
    total_steps: usize,
    interrupted: &AtomicBool,
) -> Result<()> {
    sink.emit(
        "calibration_standard_prompt",
        None,
        json!({
            "standard": standard.name().to_ascii_uppercase(),
            "step": step,
            "total_steps": total_steps,
            "instruction": calibration_standard_instruction(mode, standard)
        }),
    );
    let mut input = String::new();
    let count = io::stdin()
        .read_line(&mut input)
        .context("failed to read calibration confirmation")?;
    if interrupted.swap(false, Ordering::Relaxed) {
        bail!("calibration interrupted");
    }
    if count == 0 {
        bail!(
            "stdin closed before the {} standard was confirmed",
            standard.name()
        );
    }
    if matches!(input.trim().to_ascii_lowercase().as_str(), "q" | "quit") {
        bail!(
            "calibration aborted before the {} standard",
            standard.name()
        );
    }
    Ok(())
}

fn automatic_scan_timeouts(points: usize) -> ScanTimeouts {
    let point_budget = u64::try_from(points)
        .unwrap_or(u64::MAX)
        .saturating_mul(SCAN_POINT_BUDGET_MS);
    ScanTimeouts {
        idle: Duration::from_millis(SCAN_IDLE_TIMEOUT_MS),
        overall: Duration::from_millis(
            MIN_SCAN_OVERALL_TIMEOUT_MS.max(point_budget.saturating_add(30_000)),
        ),
    }
}

fn allocate_calibration_output(config_directory: &Path) -> Result<PathBuf> {
    let started = Instant::now();
    loop {
        let filename = format!("calibration_{}.json", Local::now().format("%y%m%d_%H%M%S"));
        let path = config_directory.join(filename);
        if !path.exists() {
            return Ok(path);
        }
        if started.elapsed() >= Duration::from_secs(2) {
            bail!(
                "could not allocate a unique calibration filename under {}",
                config_directory.display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn daemon_socket_path(config_path: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".minivna-runtime")
        });
    let runtime_directory = base.join("minivna");
    fs::create_dir_all(&runtime_directory).with_context(|| {
        format!(
            "failed to create daemon runtime directory {}",
            runtime_directory.display()
        )
    })?;
    let mut permissions = fs::metadata(&runtime_directory)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&runtime_directory, permissions)?;

    let mut hash = 0xcbf29ce484222325_u64;
    for byte in config_path.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(runtime_directory.join(format!("{hash:016x}.sock")))
}

fn run_scan_client(
    config_path: &Path,
    launch_directory: PathBuf,
    id: String,
    interrupted: &AtomicBool,
    sink: &mut EventSink,
) -> Result<()> {
    let mut stream = connect_or_start_daemon(config_path, interrupted)?;
    write_daemon_request(
        &mut stream,
        &DaemonRequest::Scan {
            id: id.clone(),
            launch_directory,
        },
    )?;

    let read_stream = stream
        .try_clone()
        .context("failed to clone daemon connection")?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || read_daemon_responses(read_stream, sender));

    loop {
        if interrupted.swap(false, Ordering::Relaxed) {
            let _ = stream.write_all(b"cancel\n");
            let _ = stream.flush();
            sink.emit(
                "cancellation_requested",
                Some(&id),
                json!({"daemon_continues_hardware_recovery": true}),
            );
            return Err(anyhow::Error::new(ReportedError {
                message: "scan cancellation requested".to_owned(),
                exit_code: 130,
            }));
        }

        match receiver.recv_timeout(Duration::from_millis(DAEMON_POLL_MS)) {
            Ok(Ok(DaemonResponse::Event { value })) => sink.emit_value(value),
            Ok(Ok(DaemonResponse::Complete { ok: true, .. })) => return Ok(()),
            Ok(Ok(DaemonResponse::Complete { ok: false, error })) => {
                return Err(anyhow::Error::new(ReportedError {
                    message: error.unwrap_or_else(|| "background scan failed".to_owned()),
                    exit_code: 1,
                }));
            }
            Ok(Err(error)) => bail!("invalid response from minivna daemon: {error}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("minivna daemon disconnected before reporting scan completion")
            }
        }
    }
}

fn read_daemon_responses(
    stream: UnixStream,
    sender: mpsc::Sender<std::result::Result<DaemonResponse, String>>,
) {
    let mut input = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => {
                let _ = sender.send(Err("unexpected end of stream".to_owned()));
                return;
            }
            Ok(_) => match serde_json::from_str(&line) {
                Ok(response) => {
                    let complete = matches!(response, DaemonResponse::Complete { .. });
                    if sender.send(Ok(response)).is_err() || complete {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(Err(error.to_string()));
                return;
            }
        }
    }
}

fn connect_or_start_daemon(config_path: &Path, interrupted: &AtomicBool) -> Result<UnixStream> {
    let socket_path = daemon_socket_path(config_path)?;
    match UnixStream::connect(&socket_path) {
        Ok(stream) => return Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to connect to daemon at {}", socket_path.display())
            });
        }
    }

    let executable = std::env::current_exe().context("failed to locate minivna executable")?;
    let mut child = ProcessCommand::new(executable);
    child
        .arg("--config")
        .arg(config_path)
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = child
        .spawn()
        .context("failed to start minivna port daemon")?;

    let started = Instant::now();
    loop {
        if interrupted.swap(false, Ordering::Relaxed) {
            bail!("interrupted while starting minivna port daemon");
        }
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            return Ok(stream);
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect minivna daemon process")?
        {
            bail!("minivna port daemon exited during startup with {status}");
        }
        if started.elapsed() >= Duration::from_millis(DAEMON_START_TIMEOUT_MS) {
            bail!(
                "timed out waiting for minivna port daemon at {}",
                socket_path.display()
            );
        }
        thread::sleep(Duration::from_millis(DAEMON_POLL_MS));
    }
}

fn write_daemon_request(stream: &mut UnixStream, request: &DaemonRequest) -> Result<()> {
    serde_json::to_writer(&mut *stream, request).context("failed to send daemon request")?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn write_daemon_response(stream: &mut UnixStream, response: &DaemonResponse) -> Result<()> {
    serde_json::to_writer(&mut *stream, response).context("failed to send daemon response")?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_daemon_request(stream: &mut UnixStream) -> Result<DaemonRequest> {
    const MAX_REQUEST_BYTES: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if bytes.len() >= MAX_REQUEST_BYTES {
            bail!("daemon request exceeds {MAX_REQUEST_BYTES} bytes");
        }
        match stream.read(&mut byte) {
            Ok(0) => bail!("client disconnected before sending a daemon request"),
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => bytes.push(byte[0]),
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => return Err(error).context("failed to read daemon request"),
        }
    }
    serde_json::from_slice(&bytes).context("invalid daemon request")
}

fn stop_daemon_if_running(config_path: &Path) -> Result<()> {
    let socket_path = daemon_socket_path(config_path)?;
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to connect to daemon at {}", socket_path.display())
            });
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set daemon shutdown timeout")?;
    write_daemon_request(&mut stream, &DaemonRequest::Shutdown)?;
    let mut input = BufReader::new(stream);
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("timed out waiting for the port daemon to stop")?;
    match serde_json::from_str::<DaemonResponse>(&line)? {
        DaemonResponse::Complete { ok: true, .. } => Ok(()),
        DaemonResponse::Complete { error, .. } => {
            bail!(error.unwrap_or_else(|| "daemon refused shutdown".to_owned()))
        }
        DaemonResponse::Event { .. } => bail!("daemon sent an event instead of shutting down"),
    }
}

struct DaemonSocketGuard(PathBuf);

impl Drop for DaemonSocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn bind_daemon_listener(socket_path: &Path) -> Result<Option<UnixListener>> {
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            return Ok(None);
        }
        fs::remove_file(socket_path).with_context(|| {
            format!(
                "failed to remove stale daemon socket {}",
                socket_path.display()
            )
        })?;
    }
    match UnixListener::bind(socket_path) {
        Ok(listener) => {
            let mut permissions = fs::metadata(socket_path)?.permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(socket_path, permissions)?;
            Ok(Some(listener))
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket_path).is_ok() {
                Ok(None)
            } else {
                Err(error).context("daemon socket is in use but not accepting connections")
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to bind daemon socket {}", socket_path.display())),
    }
}

fn run_daemon(initial_config: LoadedConfig, interrupted: &AtomicBool) -> Result<()> {
    let socket_path = daemon_socket_path(&initial_config.path)?;
    let Some(listener) = bind_daemon_listener(&socket_path)? else {
        return Ok(());
    };
    let _socket_guard = DaemonSocketGuard(socket_path);
    listener.set_nonblocking(true)?;

    let config_path = initial_config.path;
    let mut active_device_settings = initial_config.settings.device;
    let mut manager = DeviceManager::new(to_device_config(&active_device_settings));
    let idle_timeout = Duration::from_secs(PORT_IDLE_SECONDS);
    let mut last_activity = Instant::now();

    loop {
        if interrupted.swap(false, Ordering::Relaxed) || last_activity.elapsed() >= idle_timeout {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                match handle_daemon_connection(
                    &config_path,
                    &mut active_device_settings,
                    &mut manager,
                    &mut stream,
                    interrupted,
                ) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        let _ = write_daemon_response(
                            &mut stream,
                            &DaemonResponse::Complete {
                                ok: false,
                                error: Some(format!("{error:#}")),
                            },
                        );
                    }
                }
                last_activity = Instant::now();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(DAEMON_POLL_MS));
            }
            Err(error) => return Err(error).context("minivna daemon accept failed"),
        }
    }
    manager.close();
    Ok(())
}

fn handle_daemon_connection(
    config_path: &Path,
    active_device_settings: &mut DeviceSettings,
    manager: &mut DeviceManager,
    stream: &mut UnixStream,
    interrupted: &AtomicBool,
) -> Result<bool> {
    let request = read_daemon_request(stream)?;
    stream.set_read_timeout(None)?;
    match request {
        DaemonRequest::Shutdown => {
            manager.close();
            write_daemon_response(
                stream,
                &DaemonResponse::Complete {
                    ok: true,
                    error: None,
                },
            )?;
            Ok(true)
        }
        DaemonRequest::Scan {
            id,
            launch_directory,
        } => {
            let mut sink = EventSink::for_daemon_client(
                stream
                    .try_clone()
                    .context("failed to clone scan client connection")?,
            );
            let result = run_daemon_scan(
                config_path,
                active_device_settings,
                manager,
                stream,
                launch_directory,
                &id,
                interrupted,
                &mut sink,
            );
            if let Err(error) = &result {
                emit_scan_failure(&mut sink, &id, error);
            }
            write_daemon_response(
                stream,
                &DaemonResponse::Complete {
                    ok: result.is_ok(),
                    error: result.err().map(|error| format!("{error:#}")),
                },
            )?;
            Ok(false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_daemon_scan(
    config_path: &Path,
    active_device_settings: &mut DeviceSettings,
    manager: &mut DeviceManager,
    stream: &UnixStream,
    launch_directory: PathBuf,
    id: &str,
    interrupted: &AtomicBool,
    sink: &mut EventSink,
) -> Result<()> {
    let refreshed = LoadedConfig::load_or_create(config_path)?;
    if refreshed.settings.device != *active_device_settings {
        let old_port = manager.port_name().map(str::to_owned);
        manager.close();
        *active_device_settings = refreshed.settings.device.clone();
        *manager = DeviceManager::new(to_device_config(active_device_settings));
        sink.emit(
            "device_config_reloaded",
            Some(id),
            json!({
                "old_port": old_port,
                "new_port": active_device_settings.port,
                "baud": DEFAULT_BAUD
            }),
        );
    }

    let request = request_from_config(&refreshed, &launch_directory, id.to_owned())?;
    sink.emit(
        "scan_accepted",
        Some(&request.id),
        json!({
            "config": request.config_path,
            "output_directory": request.output.directory
        }),
    );
    let calibration = Calibration::load(&request.calibration_path)?;
    emit_calibration_loaded(sink, Some(&request.id), &calibration);

    let cancellation = Arc::new(AtomicBool::new(false));
    let control_finished = Arc::new(AtomicBool::new(false));
    let mut control_stream = stream
        .try_clone()
        .context("failed to clone client cancellation channel")?;
    let cancellation_reader = Arc::clone(&cancellation);
    let control_finished_reader = Arc::clone(&control_finished);
    let control_thread = thread::spawn(move || {
        let mut input = BufReader::new(&mut control_stream);
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => {
                if !control_finished_reader.load(Ordering::Relaxed) {
                    cancellation_reader.store(true, Ordering::Relaxed);
                }
            }
            Ok(_) if line.trim().eq_ignore_ascii_case("cancel") => {
                cancellation_reader.store(true, Ordering::Relaxed);
            }
            Ok(_) => {}
            Err(_) => {
                if !control_finished_reader.load(Ordering::Relaxed) {
                    cancellation_reader.store(true, Ordering::Relaxed);
                }
            }
        }
    });

    let plan = AcquisitionPlan {
        id: &request.id,
        spec: &request.spec,
        calibration: Some(&calibration),
        timeouts: request.timeouts,
        retries: request.retries,
        progress_every_points: request.progress_every_points,
        usb_reset_on_retry: request.usb_reset_on_retry,
    };
    let acquisition = acquire(manager, plan, sink, |_, _| {
        if interrupted.load(Ordering::Relaxed) {
            ScanControl::Shutdown
        } else if cancellation.load(Ordering::Relaxed) {
            ScanControl::Cancel
        } else {
            ScanControl::Continue
        }
    });
    control_finished.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Read);
    let _ = control_thread.join();

    let acquisition = acquisition?;
    finish_scan(&request, &calibration, acquisition, sink)
}

fn emit_scan_failure(sink: &mut EventSink, id: &str, error: &anyhow::Error) {
    if let Some(protocol) = error.downcast_ref::<ProtocolError>() {
        match protocol {
            ProtocolError::Cancelled { .. } | ProtocolError::Shutdown { .. } => sink.emit(
                "scan_cancelled",
                Some(id),
                json!({"error": format!("{error:#}")}),
            ),
            _ => sink.emit(
                "scan_failed",
                Some(id),
                json!({"stage": "acquisition", "error": format!("{error:#}")}),
            ),
        }
    } else {
        sink.emit(
            "scan_failed",
            Some(id),
            json!({"stage": "configuration_or_output", "error": format!("{error:#}")}),
        );
    }
}

fn request_from_config(
    loaded: &LoadedConfig,
    launch_directory: &Path,
    id: String,
) -> Result<ScanRequest> {
    if id.trim().is_empty() {
        bail!("scan id cannot be empty");
    }
    let settings = &loaded.settings;
    let spec = settings.scan_spec();
    let output = allocate_run_output(
        launch_directory,
        &settings.directory_prefix,
        settings.output,
    )?;
    Ok(ScanRequest {
        id,
        spec: spec.clone(),
        calibration_path: PathBuf::from(&settings.scan.calibration),
        config_path: loaded.path.clone(),
        config_snapshot: loaded.text.clone(),
        output,
        timeouts: automatic_scan_timeouts(spec.points),
        retries: SCAN_RETRIES,
        progress_every_points: PROGRESS_EVERY_POINTS,
        usb_reset_on_retry: true,
    })
}

fn allocate_run_output(
    launch_directory: &Path,
    prefix: &str,
    format: OutputFormat,
) -> Result<RunOutput> {
    let started = Instant::now();
    loop {
        let stem = format!("{prefix}{}", Local::now().format("%y%m%d_%H%M%S"));
        let directory = launch_directory.join(&stem);
        if !directory.exists() {
            return Ok(RunOutput {
                file: directory.join(format!("{stem}.{format}")),
                directory,
                format,
            });
        }
        if started.elapsed() >= Duration::from_secs(2) {
            bail!(
                "could not allocate a unique timestamped output directory under {}",
                launch_directory.display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn to_device_config(settings: &DeviceSettings) -> DeviceConfig {
    DeviceConfig {
        port: settings.port.clone(),
        baud: DEFAULT_BAUD,
        read_slice: Duration::from_millis(READ_SLICE_MS),
    }
}

fn acquire<F>(
    manager: &mut DeviceManager,
    plan: AcquisitionPlan<'_>,
    sink: &mut EventSink,
    mut poll: F,
) -> Result<AcquisitionResult>
where
    F: FnMut(&mut EventSink, &ScanProgress) -> ScanControl,
{
    for attempt in 0..=plan.retries {
        let was_open = manager.is_open();
        let port = match manager.ensure_open() {
            Ok(port) => port.to_owned(),
            Err(error) if attempt < plan.retries => {
                sink.emit(
                    "scan_retrying",
                    Some(plan.id),
                    json!({
                        "attempt": attempt + 1,
                        "next_attempt": attempt + 2,
                        "stage": "open",
                        "error": format!("{error:#}")
                    }),
                );
                manager.recover();
                continue;
            }
            Err(error) => return Err(error),
        };
        if !was_open {
            sink.emit(
                "port_opened",
                Some(plan.id),
                json!({"port": port, "attempt": attempt + 1}),
            );
            if let Err(error) =
                manager.require_quiet_input(Duration::from_millis(DEVICE_QUIET_WINDOW_MS))
            {
                manager.recover();
                return Err(error).context(
                    "device is not at a command boundary; refusing to treat residual scan bytes as a new response",
                );
            }
        }

        let readings = match manager
            .query_direct_readings(Duration::from_millis(TEMPERATURE_QUERY_TIMEOUT_MS))
        {
            Ok(readings) => readings,
            Err(error) if attempt < plan.retries => {
                sink.emit(
                    "scan_retrying",
                    Some(plan.id),
                    json!({
                        "attempt": attempt + 1,
                        "next_attempt": attempt + 2,
                        "stage": "direct_device_queries",
                        "error": format!("{error:#}"),
                        "port_recovery": "close_reset_and_reopen"
                    }),
                );
                manager.recover();
                if plan.usb_reset_on_retry {
                    match manager.reset_controller() {
                        Ok(firmware) => sink.emit(
                            "device_controller_reset_completed",
                            Some(plan.id),
                            json!({
                                "vid": "0403",
                                "pid": "6015",
                                "implementation": "native_chip45_serial_handshake",
                                "firmware": firmware
                            }),
                        ),
                        Err(reset_error) => {
                            sink.emit(
                                "device_controller_reset_failed",
                                Some(plan.id),
                                json!({
                                    "error": format!("{reset_error:#}"),
                                    "continuing": false
                                }),
                            );
                            return Err(reset_error).context(
                                "cannot safely retry without resetting the VNA controller",
                            );
                        }
                    }
                }
                continue;
            }
            Err(error) => {
                manager.recover();
                return Err(error).context("failed to read device temperature and supply");
            }
        };
        sink.emit(
            "device_readings",
            Some(plan.id),
            json!({
                "attempt": attempt + 1,
                "temperature_c": readings.temperature_c,
                "supply_v": readings.supply_v,
                "source": "device queries before acquisition"
            }),
        );
        sink.emit(
            "scan_started",
            Some(plan.id),
            json!({
                "attempt": attempt + 1,
                "port": port,
                "start_hz": plan.spec.start_hz,
                "stop_hz": plan.spec.stop_hz,
                "points": plan.spec.points,
                "mode": plan.spec.mode,
                "live_primary_metrics": plan.calibration.is_some(),
                "expected_bytes": plan.spec.expected_bytes()?,
                "idle_timeout_ms": plan.timeouts.idle.as_millis(),
                "overall_timeout_ms": plan.timeouts.overall.as_millis()
            }),
        );

        let mut last_reported = 0_usize;
        let mut last_elapsed_ms = 0_u128;
        let mut calibrated = plan
            .calibration
            .map(|_| Vec::with_capacity(plan.spec.points));
        let mut live_calibration_error = None;
        let result = manager.scan(plan.spec, plan.timeouts, |observation| match observation {
            ScanObservation::Progress(progress) => {
                let control = poll(sink, &progress);
                if control != ScanControl::Continue {
                    return control;
                }

                let every = plan.progress_every_points;
                while last_reported.saturating_add(every) <= progress.complete_points {
                    last_reported += every;
                    emit_progress(sink, plan.id, &progress, last_reported, last_elapsed_ms);
                    last_elapsed_ms = progress.elapsed.as_millis();
                }
                if progress.complete_points == progress.total_points
                    && last_reported != progress.total_points
                {
                    last_reported = progress.total_points;
                    emit_progress(sink, plan.id, &progress, last_reported, last_elapsed_ms);
                }
                ScanControl::Continue
            }
            ScanObservation::RawSample {
                point_index,
                total_points,
                sample,
            } => {
                let Some(calibration) = plan.calibration else {
                    return ScanControl::Continue;
                };
                match calibration.calibrate_sample(
                    plan.spec,
                    point_index,
                    &sample,
                    readings.temperature_c,
                ) {
                    Ok(calibrated_sample) => {
                        sink.emit_scan_sample(
                            plan.id,
                            attempt + 1,
                            point_index,
                            total_points,
                            &sample,
                            &calibrated_sample,
                        );
                        calibrated
                            .as_mut()
                            .expect("calibration vector exists")
                            .push(calibrated_sample);
                        ScanControl::Continue
                    }
                    Err(error) => {
                        live_calibration_error = Some(error);
                        ScanControl::Shutdown
                    }
                }
            }
        });

        if let Some(error) = live_calibration_error {
            manager.recover();
            return Err(error).context("failed to calibrate a live scan point");
        }

        match result {
            Ok(scan) => {
                if let Some(samples) = &calibrated
                    && samples.len() != scan.samples.len()
                {
                    manager.recover();
                    bail!(
                        "calibrated {} live points but received {} raw points",
                        samples.len(),
                        scan.samples.len()
                    );
                }
                return Ok(AcquisitionResult {
                    scan,
                    readings,
                    calibrated,
                });
            }
            Err(error @ (ProtocolError::Cancelled { .. } | ProtocolError::Shutdown { .. })) => {
                manager.recover();
                if plan.usb_reset_on_retry {
                    match manager.reset_controller() {
                        Ok(firmware) => sink.emit(
                            "cancelled_scan_controller_reset_completed",
                            Some(plan.id),
                            json!({
                                "vid": "0403",
                                "pid": "6015",
                                "implementation": "native_chip45_serial_handshake",
                                "firmware": firmware
                            }),
                        ),
                        Err(reset_error) => sink.emit(
                            "cancelled_scan_controller_reset_failed",
                            Some(plan.id),
                            json!({
                                "error": format!("{reset_error:#}"),
                                "warning": "the process will exit, but the instrument may continue streaming the cancelled scan until it is reset or finishes"
                            }),
                        ),
                    }
                }
                return Err(error.into());
            }
            Err(error) if attempt < plan.retries => {
                sink.emit(
                    "scan_retrying",
                    Some(plan.id),
                    json!({
                        "attempt": attempt + 1,
                        "next_attempt": attempt + 2,
                        "error": error.to_string(),
                        "port_recovery": "close_and_reopen"
                    }),
                );
                manager.recover();
                if plan.usb_reset_on_retry {
                    match manager.reset_controller() {
                        Ok(firmware) => sink.emit(
                            "device_controller_reset_completed",
                            Some(plan.id),
                            json!({
                                "vid": "0403",
                                "pid": "6015",
                                "implementation": "native_chip45_serial_handshake",
                                "firmware": firmware
                            }),
                        ),
                        Err(reset_error) => {
                            sink.emit(
                                "device_controller_reset_failed",
                                Some(plan.id),
                                json!({
                                    "error": format!("{reset_error:#}"),
                                    "continuing": false
                                }),
                            );
                            return Err(reset_error).context(
                                "cannot safely retry without resetting the VNA controller",
                            );
                        }
                    }
                }
            }
            Err(error) => {
                manager.recover();
                return Err(error.into());
            }
        }
    }
    unreachable!("attempt loop always returns")
}

fn emit_progress(
    sink: &mut EventSink,
    id: &str,
    progress: &ScanProgress,
    reported_points: usize,
    previous_elapsed_ms: u128,
) {
    let elapsed_seconds = progress.elapsed.as_secs_f64();
    let percent = reported_points as f64 * 100.0 / progress.total_points as f64;
    let byte_rate = if elapsed_seconds > 0.0 {
        progress.received_bytes as f64 / elapsed_seconds
    } else {
        0.0
    };
    sink.emit(
        "scan_progress",
        Some(id),
        json!({
            "points_received": reported_points,
            "total_points": progress.total_points,
            "bytes_received": progress.received_bytes,
            "total_bytes": progress.total_bytes,
            "percent": percent,
            "elapsed_ms": progress.elapsed.as_millis(),
            "delta_ms": progress.elapsed.as_millis().saturating_sub(previous_elapsed_ms),
            "byte_rate": byte_rate
        }),
    );
}

fn finish_scan(
    request: &ScanRequest,
    calibration: &Calibration,
    acquisition: AcquisitionResult,
    sink: &mut EventSink,
) -> Result<()> {
    let AcquisitionResult {
        scan,
        readings,
        calibrated,
    } = acquisition;
    let calibrated = match calibrated {
        Some(calibrated) => calibrated,
        None => calibration.calibrate(&request.spec, &scan.samples, readings.temperature_c)?,
    };
    emit_quality_warning(sink, Some(&request.id), request.spec.mode, &calibrated);
    match request.output.format {
        OutputFormat::Csv => {
            write_csv_atomic(&request.output.file, request.spec.mode, &calibrated)?
        }
        OutputFormat::Json => write_scan_json_atomic(
            &request.output.file,
            &request.spec,
            readings.temperature_c,
            readings.supply_v,
            "device queries before acquisition",
            &scan.samples,
            &calibrated,
        )?,
    }
    write_bytes_atomic(
        &request.output.directory.join("minivna.toml"),
        request.config_snapshot.as_bytes(),
    )?;
    sink.emit(
        "scan_completed",
        Some(&request.id),
        json!({
            "output_directory": request.output.directory,
            "output": request.output.file,
            "output_format": request.output.format,
            "config_snapshot": request.output.directory.join("minivna.toml"),
            "points": calibrated.len(),
            "bytes_received": scan.received_bytes,
            "acquisition_elapsed_ms": scan.elapsed.as_millis(),
            "temperature_c": readings.temperature_c,
            "supply_v": readings.supply_v,
            "temperature_source": "device queries before acquisition",
            "calibration": calibration.source(),
            "calibration_grid": "vnaj_headless_points_divisor"
        }),
    );
    Ok(())
}

fn emit_quality_warning(
    sink: &mut EventSink,
    id: Option<&str>,
    mode: ScanMode,
    samples: &[CalibratedSample],
) {
    if mode != ScanMode::Reflection {
        return;
    }
    let clamped = samples
        .iter()
        .filter(|sample| sample.loss_db >= 0.0 || !sample.swr.is_finite())
        .count();
    if clamped > 0 {
        sink.emit(
            "scan_quality_warning",
            id,
            json!({
                "kind": "reflection_magnitude_clamped",
                "affected_points": clamped,
                "total_points": samples.len(),
                "percent": clamped as f64 * 100.0 / samples.len() as f64,
                "meaning": "measured reflection lies at or outside the calibration unit circle; check DUT connection and calibration applicability"
            }),
        );
    }
}

fn new_scan_id() -> String {
    format!("scan-{}", Local::now().format("%Y%m%dT%H%M%S%.3f"))
}

fn emit_calibration_loaded(sink: &mut EventSink, id: Option<&str>, calibration: &Calibration) {
    sink.emit(
        "calibration_loaded",
        id,
        json!({
            "source": calibration.source(),
            "analyser_type": calibration.analyser_type(),
            "mode": calibration.mode(),
            "start_hz": calibration.start_hz(),
            "stop_hz": calibration.stop_hz(),
            "points": calibration.point_count(),
            "temperature_c": calibration.temperature_c()
        }),
    );
}

fn reset_controller(settings: &DeviceSettings, sink: &mut EventSink) -> Result<()> {
    let mut manager = DeviceManager::new(to_device_config(settings));
    let port = manager.ensure_open()?.to_owned();
    manager.close();
    let firmware = manager.reset_controller()?;
    sink.emit(
        "device_controller_reset_completed",
        None,
        json!({
            "port": port,
            "vid": "0403",
            "pid": "6015",
            "implementation": "native_chip45_serial_handshake",
            "firmware": firmware
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_requires_explicit_flag() {
        assert_eq!(output_mode(false), OutputMode::Human);
        assert_eq!(output_mode(true), OutputMode::Jsonl);
    }

    #[test]
    fn debug_is_explicit_and_global() {
        let normal = Cli::try_parse_from(["minivna", "scan"]).unwrap();
        assert!(!normal.debug);
        let debug = Cli::try_parse_from(["minivna", "scan", "--debug"]).unwrap();
        assert!(debug.debug);
    }

    #[test]
    fn normal_scan_summary_contains_no_internal_id() {
        assert_eq!(
            scan_summary(1000, 45_000_000, 60_000_000),
            "Scanning:\n\t1000 points\n\t45000000 to 60000000 Hz"
        );
    }

    #[test]
    fn debug_line_retains_internal_id_timestamp_and_point_fields() {
        let event = json!({
            "event": "scan_sample",
            "id": "scan-20260725T222204.443",
            "timestamp": "2026-07-25T22:22:04.443-07:00",
            "point": 1,
            "p1": 8601529,
            "loss_db": -6.74,
            "swr": "Infinity"
        });
        assert_eq!(debug_event_line(&event), event.to_string());
        assert_eq!(json_float(f64::INFINITY), json!("Infinity"));
        assert_eq!(json_float(f64::NEG_INFINITY), json!("-Infinity"));
        assert_eq!(json_float(f64::NAN), json!("NaN"));
    }

    #[test]
    fn live_measurement_display_is_compact_and_uses_two_decimals() {
        assert_eq!(
            live_measurement_line(18.846, -6.743_195, -37.244, false),
            " 18.85% RL:   -6.74 dB Phase:  -37.24°"
        );
    }

    #[test]
    fn live_measurement_colors_use_the_requested_truecolor_values() {
        assert_eq!(
            live_measurement_line(18.846, -6.743_195, -37.244, true),
            " 18.85% \x1b[38;2;0;174;107mRL\x1b[0m:   -6.74 dB \
             \x1b[38;2;242;40;60mPhase\x1b[0m:  -37.24°"
        );
        assert_eq!(
            bold_label("Calibration:", true),
            "\x1b[1mCalibration:\x1b[0m"
        );
    }

    #[test]
    fn path_labels_share_one_visible_path_column() {
        assert_eq!(
            aligned_path_line("config path:", "/config", false),
            "config path:      /config"
        );
        assert_eq!(
            aligned_path_line("Output directory:", "/output", false),
            "Output directory: /output"
        );
        assert_eq!(
            aligned_path_line("Calibration:", "/calibration", false),
            "Calibration:      /calibration"
        );
        assert_eq!(
            aligned_path_line("Calibration:", "/calibration", true),
            "\x1b[1mCalibration:\x1b[0m      /calibration"
        );
    }

    #[test]
    fn human_port_path_resolves_a_stable_symlink_to_the_tty_node() {
        let root =
            std::env::temp_dir().join(format!("minivna-port-display-{}", std::process::id()));
        let tty = root.join("ttyUSB7");
        let stable = root.join("usb-FTDI-test");
        fs::create_dir_all(&root).unwrap();
        fs::write(&tty, []).unwrap();
        std::os::unix::fs::symlink(&tty, &stable).unwrap();

        assert_eq!(
            human_port_path(stable.to_str().unwrap()),
            tty.display().to_string()
        );

        fs::remove_file(stable).unwrap();
        fs::remove_file(tty).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let path = config_path_from(
            Some(OsString::from("/tmp/xdg")),
            Some(OsString::from("/tmp/home")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/xdg/minivna/minivna.toml"));
    }

    #[test]
    fn config_path_falls_back_to_home() {
        let path = config_path_from(None, Some(OsString::from("/tmp/home"))).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/home/.config/minivna/minivna.toml")
        );
    }

    #[test]
    fn config_path_is_found_in_cli_arguments() {
        let arguments = vec![
            OsString::from("minivna"),
            OsString::from("scan"),
            OsString::from("--config=experiment.toml"),
        ];
        let path = config_path_from_arguments(&arguments).unwrap();
        assert_eq!(
            path,
            std::env::current_dir().unwrap().join("experiment.toml")
        );
    }
}
