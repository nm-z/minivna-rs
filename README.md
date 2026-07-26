# minivna-rs

Native miniVNA Tiny acquisition for automated data collection. The runtime is
one Rust executable; it does not launch or load Java, a JAR, Python, a shell,
or `usbreset`.

## Run

The executable creates and prints the path to:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/minivna/minivna.toml
```

With no subcommand, it prints help and performs no hardware action:

```bash
minivna
```

An explicit subcommand starts acquisition:

```bash
minivna scan
```

During a normal scan, one terminal line updates in place with completion
percentage, calibrated return loss, and calibrated phase. The display rounds
RL and phase to the same two decimal places as vna/J's CSV output. Use
`--json` when exact raw counts and full-precision values are wanted as live
machine-readable events. On an interactive terminal, `RL` is rendered in
`#00AE6B`, `Phase` in `#F2283C`, and the primary path/device labels are bold;
redirected human output remains plain text.

The normal scan header contains no internal scan ID:

```text
Scanning:
	1000 points
	45000000 to 60000000 Hz
```

`--debug` enables the intentionally noisy diagnostic stream. It prints every
event and every point, including the internal scan ID and timestamp, request
and byte progress, raw `p1` through `p4`, decoded real/imaginary values,
full-precision RL/phase, resistance, reactance, SWR, impedance, and theta:

```bash
minivna --debug scan
```

The complete default configuration is:

```toml
output = "csv"
directory_prefix = "VNA-D4_"

[device]
port = "auto"

[scan]
start = "45M"
stop = "60M"
points = "max"
mode = "reflection"
calibration = "/home/nate/.config/minivna/NATES-miniVNA_Tiny.json"
```

`start` and `stop` accept integer Hz or quoted, case-insensitive unit
strings. Supported forms include `"1000000H"`, `"1000K"`, `"45M"`, and
`"3G"`; `Hz`, `kHz`, `MHz`, and `GHz` are also accepted. Decimal unit values
such as `"45.001M"` are converted exactly and must resolve to a whole hertz.

`points` accepts an explicit integer such as `1000`, or `"max"`. The latter
uses the smaller of the range's 10 Hz command ticks and the
highest point count completed by the hardware acceptance suite. The current
verified value is 30,001 points; it is an evidence bound, not a claimed
firmware limit. The pending boundary suite will raise or replace that bound
only after complete physical scans.

`calibration` is always an absolute path to a native calibration JSON file.
On first initialization, the executable installs its supplied calibration
beside `minivna.toml` and writes that file's full path into the config. There
are no `builtin:` selectors or hidden calibration aliases.

## CSV or JSON

Select and persist the file type without starting a scan:

```bash
minivna -o json
minivna -o csv
```

This atomically persists `output = "json"` or `output = "csv"` in the TOML.
The next explicit `minivna scan` uses the saved format. A combined invocation
both persists and scans:

```bash
minivna scan -o json
```

`-o json` selects the scan file. `--json` separately selects machine-readable
JSONL terminal events:

```bash
minivna -o json --json
```

## Output

Every scan creates this under the launch working directory:

```text
./<directory_prefix><yyMMdd>_<HHmmss>/
├── <same-stem>.csv  # or .json
└── minivna.toml
```

CSV contains calibrated values. JSON is a `minivna-scan-v1` object containing
the pre-acquisition temperature and supply, the four raw ADC counts,
real/imaginary values, and all calibrated metrics. Files are published
atomically and never silently overwritten.

## Calibration

```bash
minivna calibrate
```

Reflection mode guides OPEN, SHORT, and 50-ohm LOAD sweeps. Transmission mode
guides OPEN/isolation and THRU/loopback sweeps. The result is validated, saved
beside `minivna.toml` as `calibration_<yyMMdd>_<HHmmss>.json`, and atomically
activated in `scan.calibration`.

## Process and port lifetime

Each `minivna scan` invocation is a short-lived client. It automatically starts
one detached local daemon when necessary, submits exactly one scan, relays that
scan's live output, waits for the selected file and `minivna.toml` snapshot to
be published atomically, and exits. The daemon—not the terminal command—owns
the serial port.

The daemon keeps the serial port open for one hour after a scan finishes.
Another invocation of `minivna scan` connects to the same daemon, reuses the
open port, and resets the one-hour clock. After one idle hour the daemon closes
the port and exits; a later scan starts a new daemon automatically. `calibrate`
first stops an idle daemon so it can take exclusive ownership of the
instrument.

Serial settings, adaptive deadlines, safe retry policy, per-point progress,
the startup quiet-input gate, exact calibration interpolation, and direct
device queries are internal correctness policy. Temperature and supply are
requested before every scan; a failed query fails the acquisition instead of
fabricating a calibration-temperature fallback.

`--json` changes the relayed terminal events to JSON Lines; stdin is not a
service command channel. Ctrl-C makes the client request cancellation and exit
with status 130 while the daemon performs any necessary device recovery.

See [configuration](docs/configuration.md), [JSONL protocol](docs/jsonl-protocol.md),
[wire protocol](docs/tiny-wire-protocol.md), and the
[Raspberry Pi capture investigation](docs/raspberry-pi-capture-investigation.md).

## Native runtime

The Java files under `tools/` and `tests/oracle/` are optional migration and
test-oracle utilities.
They are not compiled into, loaded by, or invoked by the Rust runtime.

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

## License

This project is licensed under the [MIT License](LICENSE). vna/J is separate
third-party software used only by optional development and migration tools and
remains subject to its own license.
