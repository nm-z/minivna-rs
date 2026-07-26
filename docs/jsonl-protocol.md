# JSONL event protocol

`--json` controls machine-readable terminal events. It is separate from
`-o json`, which selects and persists the scan file format.

Running `minivna --json scan` submits one configured scan to the automatically
managed local port daemon:

- stdout: one UTF-8 JSON event per line;
- stderr: human diagnostics only.

Without `--json`, the same events are human-readable. Configuration comes only
from TOML and is reloaded for every scan. Stdin is not a command channel. The
CLI exits after its scan output is safely published; the detached daemon keeps
the serial port open for one idle hour so the next invocation can reuse it.

## Events

A successful scan emits:

1. `config_path`
2. `scan_accepted`
3. `calibration_loaded`
4. `port_opened` unless already open
5. pre-scan `device_readings` with temperature and supply
6. `scan_started`
7. live `scan_sample` and `scan_progress`
8. optional `scan_quality_warning`
9. `scan_completed`

`calibration_loaded.source` is always the full path of the JSON calibration
file that was actually loaded.

`scan_sample` contains the exact device-returned `p1`, `p2`, `p3`, and `p4`
counts, decoded raw real/imaginary values, and full-precision calibrated
`loss_db`, `phase_deg`, resistance, reactance, SWR, impedance, and theta, plus
the point number and completion percentage.
Without `--json`, those events render as one updating line containing only the
percentage, two-decimal RL, and two-decimal phase. `scan_completed` identifies
the selected CSV or JSON output. It is followed internally by the daemon
acknowledgement that lets the CLI exit; that transport acknowledgement is not
printed as a user event.

`--debug` retains human-mode routing but prints every complete event on stderr,
including the internal scan ID, timestamps, each `scan_progress`, and each
`scan_sample`. Without `--debug`, internal scan IDs are not shown.

Scan recovery events include `cancellation_requested`, `scan_cancelled`,
`scan_failed`, and `scan_retrying`. Scan retries close and reopen the serial
port but never enter the firmware bootloader.

## JSON scan file

When `output = "json"`, the file is a `minivna-scan-v1` object containing:

- scan range, mode, and point count;
- pre-acquisition `temperature_c` and `supply_v`;
- every raw ADC count and derived real/imaginary value;
- every calibrated loss, phase, resistance, reactance, impedance, SWR, and
  theta value.

## Calibration recipe

`minivna --json calibrate` emits
`calibration_recipe_started`, `calibration_standard_prompt`,
`calibration_temperature_selected`, `calibration_standard_completed`, and
`calibration_completed`. A newline confirms each standard; `q` or `quit`
aborts.
