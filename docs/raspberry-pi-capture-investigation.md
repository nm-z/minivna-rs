# Raspberry Pi wrong-valid-frame capture investigation

## Scope and inputs

This is an analysis of the historical paired captures in:

```text
/home/nate/Desktop/Work/Roger Smith/pi-proof/pi.usbmon.txt
/home/nate/Desktop/Work/Roger Smith/pi-proof/odroid.usbmon.txt
/home/nate/Desktop/Work/Roger Smith/pi-proof/pi-java17.csv
/home/nate/Desktop/Work/Roger Smith/pi-proof/odroid-java17.csv
```

Their SHA-256 hashes are:

```text
091c052dd10bcaee2fbc91be58561748ec53ea9087249ce5eed101d0ded20384  pi.usbmon.txt
5f2070a01d77177d343b3f1b39a728033e71a2bec383ba8ebb3d6f6c84842c37  odroid.usbmon.txt
26634ae07dbce89ce233dbe22a19d435b4962fe9604d43c55639336adb6ee615  pi-java17.csv
2488f1401e4c5d5410727eed888ee54b5838c00e0f000a9d265dbbeb599a8579  odroid-java17.csv
```

Both CSVs contain 10,001 samples over 45--60 MHz. The capture analysis below
uses USB completion timestamps and declared transfer lengths, not CSV values.

## FTDI accounting

Each successful FTDI bulk-IN completion starts with two modem-status bytes.
After subtracting those two bytes from every completion, one acquisition must
contain:

```text
2 temperature bytes
+ 2 supply bytes
+ (10,001 samples * 12 bytes/sample)
= 120,016 serial bytes
```

Each capture contains exactly 240,032 serial bytes. It therefore contains two
complete acquisitions, not one long acquisition. Both captures divide exactly
at 120,016 bytes; no USB completion crosses either acquisition boundary.

The text captures print at most 32 payload bytes even when the declared
transfer length is larger. For example, many records declare 50 bytes while
only 32 are printed. Consequently, these files can prove transfer timing and
declared byte counts, but cannot reconstruct or compare every scan byte.

## Per-scan timing

The first two data-bearing completions of each acquisition are the two-byte
temperature and supply responses. Removing those leaves exactly 120,012 scan
bytes in every run:

| Host | Run | Scan transfers | Scan bytes | Scan span | Median gap | Maximum gap | Gaps over 20 ms | Bit rate |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Raspberry Pi | 1 | 2,714 | 120,012 | 43.466714 s | 16.365 ms | 16.831 ms | 0 | 22,088 bit/s |
| Raspberry Pi | 2 | 2,714 | 120,012 | 43.466378 s | 16.365 ms | 16.714 ms | 0 | 22,088 bit/s |
| Odroid | 1 | 2,717 | 120,012 | 43.466623 s | 16.003 ms | 16.149 ms | 0 | 22,088 bit/s |
| Odroid | 2 | 2,718 | 120,012 | 43.482306 s | 16.003 ms | 16.136 ms | 0 | 22,080 bit/s |

The long gaps are between acquisitions: 53.917962 seconds on the Pi and
14.842786 seconds on the Odroid. Treating each entire file as one scan
incorrectly turns those inter-run gaps into apparent in-scan USB stalls.

The first fully visible scan frame also shows that the instrument was already
producing different raw measurements, before CSV calibration or export:

| Host | Run | Raw real | Raw imaginary | Complex magnitude |
|---|---:|---:|---:|---:|
| Raspberry Pi | 1 | 1,295,255.0 | -1,309,148.5 | 1,841,617.58 |
| Raspberry Pi | 2 | 1,198,596.5 | -1,208,996.0 | 1,702,440.86 |
| Odroid | 1 | 1,915,691.5 | -1,647,367.5 | 2,526,597.24 |
| Odroid | 2 | 1,913,987.0 | -1,656,487.5 | 2,531,263.93 |

The Pi's first-frame magnitude changed by -7.56% between its two runs; the
Odroid changed by +0.18%. That is evidence of a real raw-measurement
difference, but it does not identify whether the cause was power, grounding,
RF setup, temperature, instrument state, or another analog effect.

## What the captures establish

- All four scans delivered their complete declared byte counts.
- The Pi and Odroid scan bit rates are effectively identical.
- Neither Pi scan contains an in-scan completion gap above 20 ms.
- The previously reported multi-second Pi stalls were inter-run gaps.
- The host receives a bare sequence of 12-byte samples. A sample contains four
  ADC values and no frequency, point index, sequence number, checksum, or
  frame marker.
- vna/J requests exactly `12 * points` bytes into one contiguous buffer and
  indexes that buffer in fixed 12-byte increments. USB read chunk boundaries
  do not alter its sample boundaries.

These captures contradict the proposed mechanism that Raspberry Pi USB stalls
shifted otherwise-valid frames to the wrong frequency. They do not explain the
observed magnitude difference, and they are too payload-truncated to prove
byte-for-byte equality or corruption.

## Production decision

No wrong-valid-frame detector is added. The protocol provides no invariant
that could distinguish a plausible measurement for point N from a plausible
measurement for point N+1. A continuity or ADC-range heuristic would be an
unproven scientific filter and could reject legitimate DUT behavior.

The existing startup quiet-input gate remains useful for a different,
demonstrated failure: residual bytes from an older unfinished scan. It is not
claimed to detect within-scan measurement validity.

To investigate the remaining analog discrepancy, a new controlled experiment
would need full-snaplen binary USB captures, explicit run-to-CSV pairing,
unchanged DUT/cabling, alternating hosts, and repeated scans. Until that
evidence exists, the cause remains unresolved.
