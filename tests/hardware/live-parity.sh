#!/usr/bin/env bash
set -euo pipefail

repo=/home/nate/Desktop/vnaj/minivna-rs
jar=/home/nate/Desktop/vnaJ/VNA_Headless_App/Nates-VNA-hl/vnaJ-hl.3.3.3_jp.jar
cal=/home/nate/Desktop/vnaJ/VNA_Headless_App/Nates-VNA-hl/NATES-miniVNA_Tiny.cal
config="$repo/tests/fixtures/hardware-parity-10001.toml"
start_hz=45000000
stop_hz=60000000
points=10001
expected_scan_bytes=$((points * 12))
expected_direct_bytes=$((expected_scan_bytes + 4))
expected_out_hex=31300d380d370d343530303030300d363030303030300d31303030310d0d
run_root=$(mktemp -d)
java_run="$run_root/java"
rust_run="$run_root/rust"
mkdir "$java_run" "$rust_run"

socat_pid=
monitor_pid=
rust_pid=
created_test_tty=false

cleanup() {
    if [[ -n "$monitor_pid" ]]; then
        kill "$monitor_pid" 2>/dev/null || true
    fi
    if [[ -n "$rust_pid" ]]; then
        kill "$rust_pid" 2>/dev/null || true
    fi
    if [[ -n "$socat_pid" ]]; then
        kill "$socat_pid" 2>/dev/null || true
    fi
    if [[ "$created_test_tty" == true ]]; then
        sudo -n rm -f /dev/ttyUSB99
    fi
}
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\nartifacts: %s\n' "$1" "$run_root" >&2
    exit 1
}

out_hex() {
    local endpoint=$1
    local capture=$2
    awk -v endpoint="$endpoint" '
        $3 == "S" && $4 == endpoint && / = / {
            data = 0
            for (i = 1; i <= NF; i++) {
                if ($i == "=") {
                    data = 1
                    continue
                }
                if (data && $i ~ /^[0-9a-f]+$/) {
                    printf "%s", $i
                }
            }
        }
        END { print "" }
    ' "$capture"
}

direct_bytes() {
    local endpoint=$1
    local capture=$2
    awk -v endpoint="$endpoint" '
        $3 == "C" && $4 == endpoint && $5 == 0 && $6 > 2 {
            total += $6 - 2
        }
        END { print total + 0 }
    ' "$capture"
}

scan_elapsed_us() {
    local out_endpoint=$1
    local in_endpoint=$2
    local capture=$3
    local last_out
    last_out=$(awk -v endpoint="$out_endpoint" \
        '$3 == "S" && $4 == endpoint { value = $2 } END { print value }' \
        "$capture")
    awk -v last_out="$last_out" -v endpoint="$in_endpoint" '
        $3 == "C" && $4 == endpoint && $5 == 0 && $6 > 2 {
            count++
            if (count > 2) {
                scan_bytes += $6 - 2
                last_in = $2
            }
        }
        END {
            if (scan_bytes == 0 || last_in <= last_out) {
                exit 1
            }
            print last_in - last_out
        }
    ' "$capture"
}

assert_hash() {
    local expected=$1
    local path=$2
    local actual
    actual=$(sha256sum "$path" | awk '{print $1}')
    [[ "$actual" == "$expected" ]] || fail "oracle hash mismatch for $path"
}

assert_hash 8a02ee6f9680e2c5c92a270c0ad00943a38e525de435a65a50d72b23186cec60 "$jar"
assert_hash 9e286165944eb62aea412073e0450e779638bdf7c7ed926d6dbbc40706d72f5e "$cal"
lsusb -d 0403:6015 >/dev/null || fail "FT230X 0403:6015 is not connected"
sudo -n modprobe usbmon || fail "could not load the usbmon kernel module"
usb_bus=$(lsusb -d 0403:6015 | awk '{ sub(/^0+/, "", $2); print $2 }')
usb_device=$(lsusb -d 0403:6015 | awk '{ gsub(":", "", $4); sub(/^0+/, "", $4); print $4 }')
usbmon_path="/sys/kernel/debug/usb/usbmon/${usb_bus}u"
sudo -n test -r "$usbmon_path" || fail "usbmon is unavailable for USB bus $usb_bus"
bulk_out="Bo:${usb_bus}:$(printf '%03d' "$usb_device"):2"
bulk_in="Bi:${usb_bus}:$(printf '%03d' "$usb_device"):1"
fuser /dev/ttyUSB0 >/dev/null 2>&1 && fail "/dev/ttyUSB0 is already in use"

cd "$repo"
cargo build --release
cargo test --test live_hardware real_two_point_scan_uses_both_requested_endpoints \
    -- --ignored --nocapture --test-threads=1 \
    || fail "real device failed the framed firmware-readiness control scan"

socat -x -v \
    PTY,raw,echo=0,link="$java_run/vnaj-pty" \
    FILE:/dev/ttyUSB0,b921600,raw,echo=0 \
    >"$java_run/socat.stdout" 2>"$java_run/socat.stderr" &
socat_pid=$!
for _ in $(seq 1 100); do
    [[ -L "$java_run/vnaj-pty" ]] && break
    sleep 0.02
done
[[ -L "$java_run/vnaj-pty" ]] || fail "socat did not create its test PTY"
sudo -n ln -s "$(readlink -f "$java_run/vnaj-pty")" /dev/ttyUSB99
created_test_tty=true

sudo -n timeout 90 cat "$usbmon_path" \
    >"$java_run/usbmon.txt" &
monitor_pid=$!
sleep 0.15

timeout 180s java \
    -Dpurejavacomm.log=false \
    -Dpurejavacomm.debug=false \
    -Dfstart="$start_hz" \
    -Dfstop="$stop_hz" \
    -Dfsteps="$points" \
    -DdriverId=20 \
    -Dcalfile="$cal" \
    -Dexports=csv \
    -DexportDirectory="$java_run" \
    '-DexportFilename=java_{0,date,yyMMdd}_{0,time,HHmmss}' \
    -Dscanmode=REFL \
    -DdriverPort=ttyUSB99 \
    -DkeepGeneratorOn \
    -jar "$jar" \
    >"$java_run/java.stdout" 2>"$java_run/java.stderr" \
    || fail "pinned vna/J real scan failed"

relay_size_before_invalid=$(stat -c %s "$java_run/socat.stderr")
set +e
timeout 10s java \
    -Dpurejavacomm.log=false \
    -Dpurejavacomm.debug=false \
    -Dfstart=1 \
    -Dfstop=4 \
    -Dfsteps=2 \
    -DdriverId=20 \
    -Dcalfile="$cal" \
    -Dexports=csv \
    -DexportDirectory="$java_run" \
    -DexportFilename=invalid_range_must_not_export \
    -Dscanmode=REFL \
    -DdriverPort=ttyUSB99 \
    -DkeepGeneratorOn \
    -jar "$jar" \
    >"$java_run/invalid.stdout" 2>"$java_run/invalid.stderr"
invalid_status=$?
set -e
[[ "$invalid_status" != 124 ]] || fail "vna/J 1..4/2 validation hung"
relay_size_after_invalid=$(stat -c %s "$java_run/socat.stderr")
[[ "$relay_size_after_invalid" == "$relay_size_before_invalid" ]] \
    || fail "vna/J sent device bytes for the invalid 1..4/2 range"
[[ ! -e "$java_run/invalid_range_must_not_export.csv" ]] \
    || fail "vna/J exported an invalid 1..4/2 scan"

kill "$socat_pid" 2>/dev/null || true
wait "$socat_pid" 2>/dev/null || true
socat_pid=
kill "$monitor_pid" 2>/dev/null || true
wait "$monitor_pid" 2>/dev/null || true
monitor_pid=
sudo -n rm /dev/ttyUSB99
created_test_tty=false

java_csv=$(find "$java_run" -maxdepth 1 -name '*.csv' -print -quit)
[[ -n "$java_csv" ]] || fail "vna/J produced no CSV"

sudo -n timeout 90 cat "$usbmon_path" \
    >"$rust_run/usbmon.txt" &
monitor_pid=$!
sleep 0.15
set +e
(
    cd "$rust_run"
    exec "$repo/target/release/minivna" --config "$config" scan \
        >"$rust_run/rust.stdout" 2>"$rust_run/rust.stderr"
) &
rust_pid=$!
set -e
scan_completed=false
for _ in $(seq 1 1800); do
    if grep -q '^Scan complete:' "$rust_run/rust.stderr"; then
        scan_completed=true
        break
    fi
    kill -0 "$rust_pid" 2>/dev/null || break
    sleep 0.05
done
if [[ "$scan_completed" == true ]]; then
    kill -INT "$rust_pid" 2>/dev/null || true
else
    kill -INT "$rust_pid" 2>/dev/null || true
fi
set +e
wait "$rust_pid"
rust_status=$?
set -e
rust_pid=
kill "$monitor_pid" 2>/dev/null || true
wait "$monitor_pid" 2>/dev/null || true
monitor_pid=
[[ "$scan_completed" == true ]] || fail "Rust did not complete its real scan within 90 seconds"
[[ "$rust_status" == 0 ]] || fail "Rust real scan failed"

rust_csv=$(find "$rust_run" -mindepth 2 -maxdepth 2 -name '*.csv' -print -quit)
[[ -n "$rust_csv" ]] || fail "Rust produced no CSV"

java_out=$(out_hex "$bulk_out" "$java_run/usbmon.txt")
rust_out=$(out_hex "$bulk_out" "$rust_run/usbmon.txt")
[[ "$java_out" == "$expected_out_hex" ]] || fail "vna/J outbound byte stream differs from the pinned contract"
[[ "$rust_out" == "$expected_out_hex" ]] || fail "Rust outbound byte stream differs from vna/J"

java_direct=$(direct_bytes "$bulk_in" "$java_run/usbmon.txt")
rust_direct=$(direct_bytes "$bulk_in" "$rust_run/usbmon.txt")
[[ "$java_direct" == "$expected_direct_bytes" ]] || fail "vna/J returned $java_direct direct bytes, expected $expected_direct_bytes"
[[ "$rust_direct" == "$expected_direct_bytes" ]] || fail "Rust returned $rust_direct direct bytes, expected $expected_direct_bytes"

java_elapsed=$(scan_elapsed_us "$bulk_out" "$bulk_in" "$java_run/usbmon.txt")
rust_elapsed=$(scan_elapsed_us "$bulk_out" "$bulk_in" "$rust_run/usbmon.txt")
java_bps=$((expected_scan_bytes * 8 * 1000000 / java_elapsed))
rust_bps=$((expected_scan_bytes * 8 * 1000000 / rust_elapsed))
speed_diff=$((java_bps > rust_bps ? java_bps - rust_bps : rust_bps - java_bps))
((speed_diff * 100 <= java_bps * 10)) \
    || fail "scan bitrate differs by more than 10%: Java=$java_bps Rust=$rust_bps bits/s"

java_lines=$(wc -l <"$java_csv")
rust_lines=$(wc -l <"$rust_csv")
[[ "$java_lines" == $((points + 1)) ]] || fail "vna/J CSV row count is $java_lines"
[[ "$rust_lines" == $((points + 1)) ]] || fail "Rust CSV row count is $rust_lines"
cmp <(head -n 1 "$java_csv") <(head -n 1 "$rust_csv") \
    || fail "CSV schemas differ"

java_size=$(stat -c %s "$java_csv")
rust_size=$(stat -c %s "$rust_csv")
size_diff=$((java_size > rust_size ? java_size - rust_size : rust_size - java_size))
((size_diff * 100 <= java_size * 10)) \
    || fail "CSV sizes differ by more than 10%: Java=$java_size Rust=$rust_size"

MINIVNA_JAVA_RELAY="$java_run/socat.stderr" \
MINIVNA_JAVA_CSV="$java_csv" \
MINIVNA_START_HZ="$start_hz" \
MINIVNA_STOP_HZ="$stop_hz" \
MINIVNA_POINTS="$points" \
    cargo test --test live_hardware \
    fresh_real_vnaj_scan_replays_to_bit_identical_rust_csv \
    -- --ignored --nocapture

printf 'PASS: real %s-point parity\n' "$points"
printf 'outbound bytes: %s\n' "$(( ${#expected_out_hex} / 2 ))"
printf 'direct response bytes: Java=%s Rust=%s\n' "$java_direct" "$rust_direct"
printf 'scan bitrate: Java=%s Rust=%s bits/s\n' "$java_bps" "$rust_bps"
printf 'CSV: Java=%s Rust=%s bytes, %s rows each\n' "$java_size" "$rust_size" "$java_lines"
printf 'vna/J 1..4/2: rejected before sending any device bytes\n'
printf 'artifacts: %s\n' "$run_root"
