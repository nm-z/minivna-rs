use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, de};

use crate::calibration::{DEFAULT_CALIBRATION_BYTES, DEFAULT_CALIBRATION_FILENAME};
use crate::model::{
    MAX_FREQUENCY_HZ, MAX_VERIFIED_SCAN_POINTS, MIN_FREQUENCY_HZ, MIN_SCAN_SPAN_HZ, OutputFormat,
    ScanMode, ScanSpec, max_supported_points,
};

const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../minivna.toml");
const DEFAULT_CALIBRATION_PLACEHOLDER: &str = "__MINIVNA_DEFAULT_CALIBRATION_PATH__";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub output: OutputFormat,
    pub directory_prefix: String,
    pub device: DeviceSettings,
    pub scan: ScanSettings,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceSettings {
    pub port: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanSettings {
    #[serde(
        rename = "start",
        alias = "start_hz",
        deserialize_with = "deserialize_frequency_hz"
    )]
    pub start_hz: u64,
    #[serde(
        rename = "stop",
        alias = "stop_hz",
        deserialize_with = "deserialize_frequency_hz"
    )]
    pub stop_hz: u64,
    #[serde(deserialize_with = "deserialize_points")]
    pub points: i64,
    pub mode: ScanMode,
    pub calibration: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FrequencyInput {
    Hertz(u64),
    Suffixed(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PointsInput {
    Count(i64),
    Name(String),
}

fn deserialize_points<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    match PointsInput::deserialize(deserializer)? {
        PointsInput::Count(value) => Ok(value),
        PointsInput::Name(value) if value.trim().eq_ignore_ascii_case("max") => Ok(-1),
        PointsInput::Name(value) => Err(de::Error::custom(format!(
            "scan.points string must be \"max\", got {value:?}"
        ))),
    }
}

fn deserialize_frequency_hz<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    match FrequencyInput::deserialize(deserializer)? {
        FrequencyInput::Hertz(value) => Ok(value),
        FrequencyInput::Suffixed(value) => parse_frequency_hz(&value).map_err(de::Error::custom),
    }
}

fn parse_frequency_hz(input: &str) -> std::result::Result<u64, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("frequency cannot be empty".to_owned());
    }

    let suffix_start = input
        .find(|character: char| character.is_ascii_alphabetic())
        .unwrap_or(input.len());
    let number = input[..suffix_start].trim();
    let suffix = input[suffix_start..]
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "h" | "hz" => 1_u128,
        "k" | "kh" | "khz" => 1_000,
        "m" | "mh" | "mhz" => 1_000_000,
        "g" | "gh" | "ghz" => 1_000_000_000,
        _ => {
            return Err(format!(
                "frequency {input:?} has an unknown suffix; use H, K, M, G, Hz, kHz, MHz, or GHz"
            ));
        }
    };

    let mut pieces = number.split('.');
    let whole_text = pieces.next().unwrap_or_default();
    let fractional_text = pieces.next();
    if pieces.next().is_some()
        || whole_text.is_empty()
        || !whole_text.bytes().all(|byte| byte.is_ascii_digit())
        || fractional_text.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(format!("frequency {input:?} is not a valid decimal number"));
    }

    let whole = whole_text
        .parse::<u128>()
        .map_err(|_| format!("frequency {input:?} is too large"))?;
    let (fractional, divisor) = match fractional_text {
        None => (0_u128, 1_u128),
        Some(fraction) => {
            let divisor = 10_u128
                .checked_pow(
                    u32::try_from(fraction.len())
                        .map_err(|_| format!("frequency {input:?} has too many decimal places"))?,
                )
                .ok_or_else(|| format!("frequency {input:?} has too many decimal places"))?;
            let fractional = fraction
                .parse::<u128>()
                .map_err(|_| format!("frequency {input:?} is too large"))?;
            (fractional, divisor)
        }
    };
    let scaled = whole
        .checked_mul(divisor)
        .and_then(|value| value.checked_add(fractional))
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| format!("frequency {input:?} is too large"))?;
    if scaled % divisor != 0 {
        return Err(format!(
            "frequency {input:?} does not resolve to a whole number of hertz"
        ));
    }
    u64::try_from(scaled / divisor).map_err(|_| format!("frequency {input:?} is too large"))
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub settings: AppConfig,
    pub path: PathBuf,
    pub directory: PathBuf,
    pub text: String,
    pub created: bool,
}

impl LoadedConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let directory = absolute_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let created = if absolute_path.exists() {
            false
        } else {
            fs::create_dir_all(&directory).with_context(|| {
                format!("failed to create config directory {}", directory.display())
            })?;
            let calibration_path = directory.join(DEFAULT_CALIBRATION_FILENAME);
            install_default_calibration(&calibration_path)?;
            let default_text = render_default_config(&calibration_path)?;
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&absolute_path)
                .with_context(|| {
                    format!(
                        "failed to create default config {}",
                        absolute_path.display()
                    )
                })?;
            file.write_all(default_text.as_bytes())?;
            file.sync_all()?;
            true
        };

        let mut text = fs::read_to_string(&absolute_path)
            .with_context(|| format!("failed to read config {}", absolute_path.display()))?;
        let mut settings = parse_config(&text, &absolute_path, &directory)?;
        if let Some(updated) =
            migrate_calibration_path(&text, &settings.scan.calibration, &directory)
                .map_err(|error| config_error(&absolute_path, &directory, error))?
        {
            replace_config_atomic(&absolute_path, &updated)?;
            text = updated;
            settings = parse_config(&text, &absolute_path, &directory)?;
        }
        settings
            .validate()
            .map_err(|error| config_error(&absolute_path, &directory, error))?;

        Ok(Self {
            settings,
            path: absolute_path,
            directory,
            text,
            created,
        })
    }
}

fn parse_config(text: &str, path: &Path, directory: &Path) -> Result<AppConfig> {
    toml::from_str(text).map_err(|error| config_error(path, directory, error))
}

fn config_error(path: &Path, directory: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "invalid configuration {}\n{error}\n\n{}",
        path.display(),
        valid_configuration_help(directory)
    )
}

fn valid_configuration_help(directory: &Path) -> String {
    format!(
        "Valid values:\n  output = \"csv\" or \"json\"\n  directory_prefix = a filename-safe prefix\n  device.port = \"auto\" or a serial-device path\n  scan.start = {MIN_FREQUENCY_HZ}..={} Hz\n  scan.stop = start + at least {MIN_SCAN_SPAN_HZ} Hz, up to {MAX_FREQUENCY_HZ} Hz\n  scan.points = \"max\" or an integer from 2 through the range-specific limit (at most {MAX_VERIFIED_SCAN_POINTS})\n  scan.mode = \"reflection\" or \"transmission\"\n  scan.calibration = an absolute JSON file path, for example {}",
        MAX_FREQUENCY_HZ - MIN_SCAN_SPAN_HZ,
        directory.join(DEFAULT_CALIBRATION_FILENAME).display()
    )
}

fn render_default_config(calibration_path: &Path) -> Result<String> {
    let calibration_path = calibration_path
        .to_str()
        .context("default calibration path is not valid UTF-8")?;
    let quoted_path = serde_json::to_string(calibration_path)?;
    Ok(DEFAULT_CONFIG_TEMPLATE.replace(DEFAULT_CALIBRATION_PLACEHOLDER, &quoted_path))
}

fn install_default_calibration(path: &Path) -> Result<()> {
    match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(mut file) => {
            file.write_all(DEFAULT_CALIBRATION_BYTES)?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to install calibration {}", path.display()))
        }
    }
}

fn migrate_calibration_path(
    text: &str,
    configured: &str,
    directory: &Path,
) -> Result<Option<String>> {
    let configured_path = Path::new(configured);
    if configured_path.is_absolute() {
        return Ok(None);
    }
    if configured.starts_with("builtin:") {
        bail!(
            "unsupported calibration selector {configured:?}; scan.calibration must be an absolute JSON file path"
        );
    }
    let calibration_path = directory.join(configured_path);
    let calibration_text = calibration_path
        .to_str()
        .context("calibration path is not valid UTF-8")?;
    Ok(Some(replace_scan_calibration(text, calibration_text)?))
}

pub fn set_calibration_path(path: &Path, calibration_path: &Path) -> Result<()> {
    if !calibration_path.is_absolute() {
        bail!(
            "calibration path must be absolute, got {}",
            calibration_path.display()
        );
    }
    let calibration_text = calibration_path
        .to_str()
        .context("calibration path is not valid UTF-8")?;
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let updated = replace_scan_calibration(&text, calibration_text)?;
    let settings: AppConfig = toml::from_str(&updated)
        .with_context(|| format!("updated configuration {} is invalid", path.display()))?;
    settings.validate()?;
    if settings.scan.calibration != calibration_text {
        bail!("updated configuration did not retain the new calibration path");
    }

    replace_config_atomic(path, &updated)
}

pub fn set_output_format(path: &Path, format: OutputFormat) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let updated = replace_root_output(&text, format)?;
    let settings: AppConfig = toml::from_str(&updated)
        .with_context(|| format!("updated configuration {} is invalid", path.display()))?;
    settings.validate()?;
    if settings.output != format {
        bail!("updated configuration did not retain output = {format:?}");
    }
    replace_config_atomic(path, &updated)
}

fn replace_scan_calibration(text: &str, calibration_path: &str) -> Result<String> {
    let quoted_path = serde_json::to_string(calibration_path)?;
    let mut in_scan = false;
    let mut replaced = false;
    let mut output = String::with_capacity(text.len() + calibration_path.len());

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_scan = trimmed == "[scan]";
        }
        if in_scan
            && let Some((key, _)) = line.split_once('=')
            && key.trim() == "calibration"
        {
            if replaced {
                bail!("configuration contains duplicate scan.calibration keys");
            }
            let indentation = &line[..line.len() - line.trim_start().len()];
            let newline = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            output.push_str(indentation);
            output.push_str("calibration = ");
            output.push_str(&quoted_path);
            output.push_str(newline);
            replaced = true;
        } else {
            output.push_str(line);
        }
    }

    if !replaced {
        bail!("configuration has no scan.calibration key to update");
    }
    Ok(output)
}

fn replace_root_output(text: &str, format: OutputFormat) -> Result<String> {
    let mut in_root = true;
    let mut replaced = false;
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_root = false;
        }
        if in_root
            && let Some((key, _)) = line.split_once('=')
            && key.trim() == "output"
        {
            if replaced {
                bail!("configuration contains duplicate output keys");
            }
            let newline = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            output.push_str(&format!("output = \"{format}\"{newline}"));
            replaced = true;
        } else {
            output.push_str(line);
        }
    }
    if !replaced {
        bail!("configuration has no root output key to update");
    }
    Ok(output)
}

fn replace_config_atomic(path: &Path, updated: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .context("configuration path has no filename")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "failed to create temporary configuration {}",
                    temporary.display()
                )
            })?;
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace configuration {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        if self.device.port.trim().is_empty() {
            bail!("device.port cannot be empty");
        }
        if self.scan.points != -1 && self.scan.points < 2 {
            bail!("scan.points must be \"max\" or an integer of at least 2");
        }
        self.scan_spec().validate()?;
        if self.scan.calibration.trim().is_empty() {
            bail!("scan.calibration must be an absolute JSON file path; it cannot be empty");
        }
        if self.scan.calibration.starts_with("builtin:") {
            bail!(
                "scan.calibration no longer accepts builtin selectors; use the full path to the installed JSON calibration file"
            );
        }
        if !Path::new(&self.scan.calibration).is_absolute() {
            bail!(
                "scan.calibration must be an absolute JSON file path, got {:?}",
                self.scan.calibration
            );
        }
        let prefix = &self.directory_prefix;
        if prefix.contains('/') || prefix.contains('\\') || prefix.contains('\0') {
            bail!("directory_prefix must be a filename prefix, not a path");
        }
        Ok(())
    }

    pub fn scan_spec(&self) -> ScanSpec {
        let points = if self.scan.points == -1 {
            max_supported_points(self.scan.start_hz, self.scan.stop_hz)
        } else {
            usize::try_from(self.scan.points).unwrap_or(0)
        };
        ScanSpec {
            start_hz: self.scan.start_hz,
            stop_hz: self.scan.stop_hz,
            points,
            mode: self.scan.mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_text() -> String {
        render_default_config(Path::new("/tmp/minivna-test/NATES-miniVNA_Tiny.json")).unwrap()
    }

    fn temporary_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "minivna-config-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn bundled_config_is_valid() {
        let config: AppConfig = toml::from_str(&default_text()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.output, OutputFormat::Csv);
        assert_eq!(config.scan.start_hz, 45_000_000);
        assert_eq!(config.scan.stop_hz, 60_000_000);
        assert_eq!(config.scan.points, -1);
        assert_eq!(config.scan_spec().points, 30_001);
    }

    #[test]
    fn points_accepts_max_or_an_explicit_integer() {
        let text = default_text();
        let maximum: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(maximum.scan.points, -1);
        assert_eq!(maximum.scan_spec().points, 30_001);

        let fixed_text = text.replace("points = \"max\"", "points = 1000");
        let fixed: AppConfig = toml::from_str(&fixed_text).unwrap();
        assert_eq!(fixed.scan.points, 1000);
        assert_eq!(fixed.scan_spec().points, 1000);

        let invalid_text = text.replace("points = \"max\"", "points = \"lots\"");
        assert!(toml::from_str::<AppConfig>(&invalid_text).is_err());
    }

    #[test]
    fn frequency_suffixes_are_exact_integer_hertz() {
        for (text, expected) in [
            ("1000000H", 1_000_000),
            ("1000K", 1_000_000),
            ("45M", 45_000_000),
            ("45.001 MHz", 45_001_000),
            ("3G", 3_000_000_000),
        ] {
            assert_eq!(parse_frequency_hz(text).unwrap(), expected, "{text}");
        }
        assert!(parse_frequency_hz("1.0000001M").is_err());
        assert!(parse_frequency_hz("45 bananas").is_err());
    }

    #[test]
    fn legacy_frequency_keys_and_integer_values_remain_compatible() {
        let text = default_text()
            .replace("start = \"45M\"", "start_hz = 45000000")
            .replace("stop = \"60M\"", "stop_hz = 60000000")
            .replace("points = \"max\"", "points = -1");
        let config: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.scan.start_hz, 45_000_000);
        assert_eq!(config.scan.stop_hz, 60_000_000);
        assert_eq!(config.scan.points, -1);
    }

    #[test]
    fn prefix_cannot_escape_working_directory() {
        let mut config: AppConfig = toml::from_str(&default_text()).unwrap();
        config.directory_prefix = "../escape".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn calibration_path_update_preserves_the_rest_of_the_file() {
        let updated = replace_scan_calibration(
            &default_text(),
            "/tmp/minivna-test/calibration_260723_220000.json",
        )
        .unwrap();
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(
            config.scan.calibration,
            "/tmp/minivna-test/calibration_260723_220000.json"
        );
        assert!(updated.contains("# Every run creates"));
    }

    #[test]
    fn output_format_update_changes_only_the_root_setting() {
        let updated = replace_root_output(&default_text(), OutputFormat::Json).unwrap();
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(config.output, OutputFormat::Json);
        assert!(updated.contains("calibration = \"/tmp/minivna-test/NATES-miniVNA_Tiny.json\""));
    }

    #[test]
    fn first_run_installs_calibration_and_writes_its_absolute_path() {
        let root = temporary_root("first-run");
        let config_path = root.join("minivna.toml");
        let loaded = LoadedConfig::load_or_create(&config_path).unwrap();
        let calibration_path = root.join(DEFAULT_CALIBRATION_FILENAME);

        assert!(loaded.created);
        assert_eq!(
            loaded.settings.scan.calibration,
            calibration_path.to_str().unwrap()
        );
        assert_eq!(
            fs::read(&calibration_path).unwrap(),
            DEFAULT_CALIBRATION_BYTES
        );
        assert!(
            fs::read_to_string(&config_path)
                .unwrap()
                .contains(calibration_path.to_str().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_calibration_path_is_rewritten_as_a_full_path() {
        let root = temporary_root("relative-path");
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("minivna.toml");
        let default_path = root.join(DEFAULT_CALIBRATION_FILENAME);
        let intended_path = root.join("custom-calibration.json");
        let relative = render_default_config(&default_path)
            .unwrap()
            .replace(default_path.to_str().unwrap(), "custom-calibration.json");
        fs::write(&config_path, relative).unwrap();

        let loaded = LoadedConfig::load_or_create(&config_path).unwrap();
        assert!(!loaded.created);
        assert_eq!(
            loaded.settings.scan.calibration,
            intended_path.to_str().unwrap()
        );
        assert!(loaded.text.contains(intended_path.to_str().unwrap()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn calibration_selectors_are_rejected() {
        let error = migrate_calibration_path(
            &default_text(),
            "builtin:anything",
            Path::new("/tmp/minivna-test"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must be an absolute JSON file path"));
    }

    #[test]
    fn invalid_toml_error_lists_every_public_value_domain() {
        let error = parse_config(
            &default_text().replace("output = \"csv\"", "output = \"xml\""),
            Path::new("/tmp/minivna-test/minivna.toml"),
            Path::new("/tmp/minivna-test"),
        )
        .unwrap_err()
        .to_string();
        for expected in [
            "output = \"csv\" or \"json\"",
            "scan.start = 1000000..=2999999000 Hz",
            "scan.stop = start + at least 1000 Hz, up to 3000000000 Hz",
            "scan.points = \"max\" or an integer",
            "scan.mode = \"reflection\" or \"transmission\"",
            "scan.calibration = an absolute JSON file path",
        ] {
            assert!(
                error.contains(expected),
                "missing {expected:?} from {error}"
            );
        }
    }
}
