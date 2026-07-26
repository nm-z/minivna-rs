use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

use crate::model::ScanSpec;
use crate::protocol::{
    ProtocolError, ScanControl, ScanObservation, ScanResult, ScanTimeouts, scan_io,
};

pub const DEFAULT_BAUD: u32 = 921_600;
const BOOTLOADER_BAUD: u32 = 230_400;
const FTDI_VID: u16 = 0x0403;
const FT230X_PID: u16 = 0x6015;

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub port: String,
    pub baud: u32,
    pub read_slice: Duration,
}

pub struct DeviceManager {
    config: DeviceConfig,
    resolved_port: Option<String>,
    port: Option<Box<dyn SerialPort>>,
}

#[derive(Clone, Copy, Debug)]
pub struct DirectReadings {
    pub temperature_c: f64,
    pub supply_v: f64,
}

impl DeviceManager {
    pub fn new(config: DeviceConfig) -> Self {
        Self {
            config,
            resolved_port: None,
            port: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.port.is_some()
    }

    pub fn port_name(&self) -> Option<&str> {
        self.resolved_port.as_deref()
    }

    pub fn ensure_open(&mut self) -> Result<&str> {
        if self.port.is_none() {
            let port_name = resolve_port(&self.config.port)?;
            let port = serialport::new(&port_name, self.config.baud)
                .data_bits(DataBits::Eight)
                .flow_control(FlowControl::None)
                .parity(Parity::None)
                .stop_bits(StopBits::One)
                .timeout(self.config.read_slice)
                .open()
                .with_context(|| {
                    format!(
                        "failed to open {port_name} at {} baud; another process may own it",
                        self.config.baud
                    )
                })?;
            port.clear(ClearBuffer::All)
                .with_context(|| format!("failed to clear serial buffers on {port_name}"))?;
            self.resolved_port = Some(port_name);
            self.port = Some(port);
        }
        Ok(self.resolved_port.as_deref().expect("resolved open port"))
    }

    pub fn scan<F>(
        &mut self,
        spec: &ScanSpec,
        timeouts: ScanTimeouts,
        observe: F,
    ) -> Result<ScanResult, ProtocolError>
    where
        F: FnMut(ScanObservation) -> ScanControl,
    {
        let port = self.port.as_mut().expect("ensure_open before scan");
        port.clear(ClearBuffer::All)
            .map_err(|source| ProtocolError::Io {
                received: 0,
                expected: spec.expected_bytes().unwrap_or(0),
                source: io::Error::other(source.to_string()),
            })?;
        scan_io(port.as_mut(), spec, timeouts, observe)
    }

    pub fn query_temperature(&mut self, timeout: Duration) -> Result<f64> {
        Ok(f64::from(self.query_u16(b"10\r", "temperature", timeout)?) / 10.0)
    }

    pub fn query_supply(&mut self, timeout: Duration) -> Result<f64> {
        Ok(f64::from(self.query_u16(b"8\r", "supply", timeout)?) * 6.0 / 1024.0)
    }

    pub fn query_direct_readings(&mut self, timeout: Duration) -> Result<DirectReadings> {
        let temperature_c = self.query_temperature(timeout)?;
        let supply_v = self.query_supply(timeout)?;
        Ok(DirectReadings {
            temperature_c,
            supply_v,
        })
    }

    pub fn query_firmware(&mut self, timeout: Duration) -> Result<String> {
        let port = self.port.as_mut().expect("ensure_open before query");
        port.clear(ClearBuffer::All)
            .context("failed to clear input before firmware query")?;
        port.write_all(b"9\r")
            .context("failed to write firmware query")?;
        port.flush().context("failed to flush firmware query")?;
        let firmware = read_line(port.as_mut(), timeout, "firmware query")?;
        if !firmware.starts_with("FW Tiny ") {
            bail!("unexpected firmware identity {firmware:?}");
        }
        Ok(firmware)
    }

    pub fn require_quiet_input(&mut self, quiet_window: Duration) -> Result<()> {
        let port = self
            .port
            .as_mut()
            .expect("ensure_open before readiness check");
        port.clear(ClearBuffer::Input)
            .context("failed to clear input before readiness check")?;
        let started = Instant::now();
        let mut bytes = [0_u8; 64];
        while started.elapsed() < quiet_window {
            match port.read(&mut bytes) {
                Ok(0) => {}
                Ok(count) => {
                    let preview = bytes[..count.min(8)]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    bail!(
                        "received {count} unsolicited byte(s) while waiting for an idle device \
                         (first bytes: {preview}); an older scan is still streaming"
                    );
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("device readiness read failed"),
            }
        }
        Ok(())
    }

    /// Requests the miniVNA Tiny's Chip45 bootloader and restarts the
    /// application. The application only parses this request at a command
    /// boundary, so it cannot be relied upon to interrupt an active scan.
    /// Success requires a subsequent application-firmware identity response.
    pub fn reset_controller(&mut self) -> Result<String> {
        self.ensure_open()?;
        let port = self.port.as_mut().expect("ensure_open before reset");
        port.clear(ClearBuffer::All)
            .context("failed to clear serial buffers before controller reset")?;
        port.write_all(b"99\r")
            .context("failed to send miniVNA controller reset command")?;
        port.flush()
            .context("failed to flush miniVNA controller reset command")?;
        thread::sleep(Duration::from_millis(50));
        port.set_baud_rate(BOOTLOADER_BAUD)
            .context("failed to select miniVNA bootloader baud rate")?;

        let reset_result = (|| -> Result<()> {
            port.clear(ClearBuffer::Input)
                .context("failed to clear input before bootloader synchronization")?;
            for _ in 0..100 {
                port.write_all(b"U")
                    .context("failed to send bootloader synchronization byte")?;
                port.flush()
                    .context("failed to flush bootloader synchronization byte")?;
                thread::sleep(Duration::from_millis(10));
            }
            read_until_byte(
                port.as_mut(),
                b'>',
                Duration::from_secs(3),
                "bootloader prompt",
            )?;
            port.write_all(b"\n")
                .context("failed to enter bootloader command mode")?;
            port.flush()
                .context("failed to flush bootloader command-mode request")?;
            read_until_byte(
                port.as_mut(),
                b'-',
                Duration::from_secs(3),
                "bootloader command-mode acknowledgement",
            )?;
            port.write_all(b"g\n")
                .context("failed to send bootloader start-application command")?;
            port.flush()
                .context("failed to flush bootloader start-application command")?;
            // Some Tiny V1.0 units reset immediately and omit the `g+` echo
            // expected by vna/J's flasher. The application firmware identity
            // queried below is the stronger reset-success criterion.
            thread::sleep(Duration::from_millis(250));
            Ok(())
        })();

        port.set_baud_rate(DEFAULT_BAUD)
            .context("failed to restore miniVNA application baud rate")?;
        thread::sleep(Duration::from_millis(500));
        port.clear(ClearBuffer::All)
            .context("failed to clear buffers after controller reset")?;
        reset_result?;

        port.write_all(b"9\r")
            .context("failed to query firmware after controller reset")?;
        port.flush()
            .context("failed to flush firmware query after controller reset")?;
        let firmware = read_line(port.as_mut(), Duration::from_secs(2), "firmware query")?;
        if !firmware.starts_with("FW Tiny ") {
            bail!("controller reset returned unexpected firmware identity {firmware:?}");
        }
        Ok(firmware)
    }

    fn query_u16(&mut self, command: &[u8], metric: &str, timeout: Duration) -> Result<u16> {
        let port = self.port.as_mut().expect("ensure_open before query");
        port.clear(ClearBuffer::All)
            .with_context(|| format!("failed to clear input before {metric} query"))?;
        port.write_all(command)
            .with_context(|| format!("failed to write {metric} query"))?;
        port.flush()
            .with_context(|| format!("failed to flush {metric} query"))?;

        let started = Instant::now();
        let mut response = [0_u8; 2];
        let mut received = 0;
        while received < response.len() {
            if started.elapsed() >= timeout {
                bail!(
                    "{metric} query timed out after {} ms ({received}/2 bytes)",
                    timeout.as_millis()
                );
            }
            match port.read(&mut response[received..]) {
                Ok(0) => {}
                Ok(count) => received += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("{metric} query read failed"));
                }
            }
        }
        Ok(u16::from_le_bytes(response))
    }

    pub fn close(&mut self) {
        if let Some(port) = self.port.take() {
            let _ = port.clear(ClearBuffer::All);
            drop(port);
        }
    }

    pub fn recover(&mut self) {
        self.close();
    }
}

impl Drop for DeviceManager {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn resolve_port(requested: &str) -> Result<String> {
    if requested != "auto" {
        return Ok(requested.to_owned());
    }

    let by_id = Path::new("/dev/serial/by-id");
    if let Ok(entries) = fs::read_dir(by_id) {
        let mut matches: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("FTDI_FT230X_Basic_UART"))
            })
            .collect();
        matches.sort();
        match matches.as_slice() {
            [only] => return Ok(only.display().to_string()),
            [] => {}
            many => {
                let names = many
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("multiple FT230X devices found ({names}); select one with --port");
            }
        }
    }

    let mut matches = Vec::new();
    for info in serialport::available_ports().context("failed to enumerate serial ports")? {
        if let serialport::SerialPortType::UsbPort(usb) = info.port_type
            && usb.vid == FTDI_VID
            && usb.pid == FT230X_PID
        {
            matches.push(info.port_name);
        }
    }
    matches.sort();
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(anyhow!(
            "no miniVNA FT230X serial port found; connect it or pass --port explicitly"
        )),
        many => Err(anyhow!(
            "multiple FT230X ports found ({}); select one with --port",
            many.join(", ")
        )),
    }
}

pub fn list_ports() -> Result<Vec<serialport::SerialPortInfo>> {
    serialport::available_ports().context("failed to enumerate serial ports")
}

fn read_until_byte(
    port: &mut dyn SerialPort,
    expected: u8,
    timeout: Duration,
    description: &str,
) -> Result<()> {
    let started = Instant::now();
    let mut byte = [0_u8; 1];
    while started.elapsed() < timeout {
        match port.read(&mut byte) {
            Ok(1) if byte[0] == expected => return Ok(()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed while reading {description}"));
            }
        }
    }
    bail!(
        "timed out after {} ms waiting for {description} byte 0x{expected:02x}",
        timeout.as_millis()
    )
}

fn read_line(port: &mut dyn SerialPort, timeout: Duration, description: &str) -> Result<String> {
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while started.elapsed() < timeout {
        match port.read(&mut byte) {
            Ok(1) if matches!(byte[0], b'\r' | b'\n') && !bytes.is_empty() => {
                return match String::from_utf8(bytes) {
                    Ok(line) => Ok(line),
                    Err(error) => {
                        let encoded = error
                            .into_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        bail!("{description} was not valid UTF-8: {encoded}")
                    }
                };
            }
            Ok(1) if !matches!(byte[0], b'\r' | b'\n') => bytes.push(byte[0]),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed while reading {description}"));
            }
        }
    }
    bail!(
        "timed out after {} ms waiting for {description}",
        timeout.as_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_port_does_not_require_discovery() {
        assert_eq!(resolve_port("/dev/example").unwrap(), "/dev/example");
    }
}
