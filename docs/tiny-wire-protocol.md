# miniVNA Tiny wire protocol used here

Serial configuration is fixed at 921,600 baud, 8 data bits, no parity, one stop
bit, and no flow control.

## Scan request

One scan is five ASCII fields, each terminated by carriage return:

1. mode: `7` reflection or `6` transmission;
2. start frequency in Hz divided by 10;
3. stop frequency in Hz divided by 10;
4. number of samples;
5. an empty spare field.

With the default range and `points = "max"`, the exact request is:

```text
7\r4500000\r6000000\r30001\r\r
```

The firmware returns exactly `points * 12` binary bytes and no header.

## Sample frame

Each frame holds four unsigned little-endian 24-bit values:

```text
offset 0..2   p1
offset 3..5   p3
offset 6..8   p2
offset 9..11  p4
```

The raw complex measurement is calculated only after receipt:

```text
real      = (p1 - p2) / 2
imaginary = (p3 - p4) / 2
```

There is no frequency, point index, sequence number, checksum, or frame marker
in a sample. Once a structurally complete 12-byte sample has been received,
the protocol itself provides no way to prove that it belongs to one requested
frequency rather than an adjacent one. See the
[Raspberry Pi capture investigation](raspberry-pi-capture-investigation.md).

## Direct device readings

Before each scan, command `10\r` requests a two-byte little-endian temperature
in tenths of a degree C, and command `8\r` requests a two-byte little-endian
supply reading scaled by `6 / 1024`. This ordering matches the pinned vna/J
headless driver. A failed query fails the acquisition; no calibration
temperature is fabricated as a fallback.

## Cancellation

The Tiny protocol has no verified in-scan cancel command. Closing or resetting
the FT230X USB bridge does not stop the VNA controller: it can continue
streaming structurally valid frames. The hardware acceptance suite requires a
framed `FW Tiny ...` response to command `9\r` before it sends any test scan;
arbitrary residual bytes do not count as readiness. Production acquisition
preserves the vna/J outbound scan sequence, so it instead requires a quiet
input window before its temperature and supply queries and refuses any
unsolicited residual bytes it observes.

The controller also reads its reset request only at a command boundary. A
controller already executing a scan may therefore require a physical power
disconnect before another acquisition is safe.
