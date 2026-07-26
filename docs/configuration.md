# Configuration reference

The executable creates and uses:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/minivna/minivna.toml
```

The complete default file is:

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

- `output`: `"csv"` or `"json"`. `minivna -o csv` and `minivna -o json`
  atomically update this line before scanning.
- `directory_prefix`: filename-safe prefix before mandatory
  `yyMMdd_HHmmss`.
- `device.port`: stable tty path or `"auto"` for the single FT230X.
- `scan.start`, `scan.stop`: requested frequency endpoints. Each accepts an
  integer number of Hz or a quoted unit string such as `"45M"` or `"60MHz"`.
- `scan.points`: explicit integer point count, or `"max"` for the smaller of the range's
  10 Hz command ticks and the highest point count completed by the hardware
  suite. That verified count is currently 30,001; it is an evidence bound, not
  a claimed firmware limit. It changes only after a complete physical scan.
- `scan.mode`: `"reflection"` or `"transmission"`.
- `scan.calibration`: absolute path to a native calibration JSON file. On
  first initialization, the supplied calibration is installed beside this
  TOML and its full path is written here. Selectors and relative paths are not
  retained.

The serial settings, automatic deadlines, startup quiet-input gate, progress
reporting, pre-acquisition temperature and supply queries, exact calibration
interpolation, detached port ownership, and one-hour port idle behavior are
internal correctness policy rather than user configuration. The hardware test
suite separately requires an exact `FW Tiny ...` response before every test
scan.

If TOML parsing or validation fails, the process exits with status 1 before it
connects to the daemon or hardware. The error identifies the bad line or value
and prints the valid outputs, modes, frequency ranges, point forms, and
absolute-calibration-path requirement.

`minivna calibrate` runs the appropriate standard recipe, writes a timestamped
native calibration beside this TOML, and atomically updates
`scan.calibration` after validation.
