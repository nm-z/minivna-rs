# Live vna/J GUI observations

## 2026-07-25: vnaJ 3.4.8 direct USB, full-range scan

- Requested range: 1,000,000 through 2,999,999,426 Hz.
- vnaJ requested 1,382 samples and logged a 2,171,614 Hz increment.
- vnaJ expected 16,584 payload bytes (`1,382 * 12`).
- The instrument returned zero measurement payload bytes.
- Completed scan points: **0**. The logged `Steps 1382` describes the
  request, not completed acquisition progress.
- vnaJ remained blocked in `receiveBytestream(16584)` instead of timing out or
  reporting zero completed points.
- Before the scan, temperature and supply queries each timed out after 20
  seconds and were replaced with the fallback values `0.0` and `-1.0`.
- While vnaJ owned `/dev/ttyUSB0`, the FT230X was physically configured at
  approximately 4,103 baud rather than the requested 921,600 baud.
