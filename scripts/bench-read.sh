#!/usr/bin/env bash
# bench-read.sh — Repeatable fio read-only benchmarks for a Rift FUSE mount.
#
# Usage:
#   ./scripts/bench-read.sh [OPTIONS]
#
# Options:
#   -m, --mount DIR       Rift FUSE mount point       (default: /tmp/rift-mount)
#   -s, --share DIR       Local share dir for test data (default: /tmp/rift-share)
#   -t, --runtime SECS    Per-test duration in seconds (default: 30)
#   -j, --jobs N          Concurrent jobs for multi tests (default: 4)
#   -f, --filesize SIZE   Test file size               (default: 1G)
#   -n, --smallfiles N    Number of small test files    (default: 100)
#   -o, --output DIR      Output directory for results  (default: bench-results/<timestamp>)
#   -k, --keep-data       Keep test data on share after bench run
#   --quick               Run each test for 10s instead of full duration
#   --only SUITE          Only run the named suite(s), comma-separated.
#                          Suites: sequential, random, multi, rand-multi, small-files
#   -h, --help            Show this help
#
# The script will:
#   1. Verify the mount is alive
#   2. Create test data on the *share* dir (server-side) so reads go through Rift
#   3. Run each fio suite, saving JSON + human-readable output
#   4. Print a summary table to stdout
#
# Test data layout on the share:
#   <share>/bench-read/seqfile        — large file for sequential/random tests
#   <share>/bench-read/small_NNN.dat   — small files for metadata tests

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIO_DIR="${SCRIPT_DIR}/fio"

# ── Defaults ────────────────────────────────────────────────────────────────
MOUNT="${MOUNT:-/tmp/rift-mount}"
SHARE="${SHARE:-/tmp/rift-share}"
RUNTIME="${RUNTIME:-30}"
NUMJOBS="${NUMJOBS:-4}"
FILESIZE="${FILESIZE:-1G}"
NRFILES="${NRFILES:-100}"
OUTPUT=""
KEEP_DATA=false
QUICK=false
ONLY=""

# ── Parse args ──────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    -m|--mount)      MOUNT="$2";     shift 2 ;;
    -s|--share)      SHARE="$2";      shift 2 ;;
    -t|--runtime)    RUNTIME="$2";    shift 2 ;;
    -j|--jobs)       NUMJOBS="$2";    shift 2 ;;
    -f|--filesize)   FILESIZE="$2";   shift 2 ;;
    -n|--smallfiles)  NRFILES="$2";   shift 2 ;;
    -o|--output)     OUTPUT="$2";     shift 2 ;;
    -k|--keep-data)  KEEP_DATA=true;  shift ;;
    --quick)         QUICK=true;      shift ;;
    --only)          ONLY="$2";       shift 2 ;;
    -h|--help)
      sed -n '3,/^$/p' "$0"
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

if [[ "$QUICK" == true ]]; then
  RUNTIME=10
fi

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
OUTPUT="${OUTPUT:-bench-results/${TIMESTAMP}}"
BENCH_DIR="bench-read"

# ── Helpers ──────────────────────────────────────────────────────────────────
info()  { printf '[INFO] %s\n' "$1"; }
warn()  { printf '[WARN] %s\n' "$1" >&2; }
die()   { printf '[FATAL] %s\n' "$1" >&2; exit 1; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
bold()  { printf '\033[1m%s\033[0m\n' "$1"; }

# ── Preflight ────────────────────────────────────────────────────────────────
info "Rift read-only benchmark"
info "  mount       = ${MOUNT}"
info "  share       = ${SHARE}"
info "  runtime     = ${RUNTIME}s"
info "  filesize    = ${FILESIZE}"
info "  numjobs     = ${NUMJOBS}"
info "  smallfiles  = ${NRFILES}"
info "  output      = ${OUTPUT}"
echo ""

command -v fio >/dev/null || die "fio not found — install with: sudo apt install fio"
command -v jq >/dev/null  || die "jq not found — install with: sudo apt install jq"

if ! mountpoint -q "${MOUNT}" 2>/dev/null; then
  die "Mount point ${MOUNT} is not a live FUSE mount. Start Rift and mount first."
fi
green "Mount ${MOUNT} is alive."

[[ -d "${SHARE}" ]] || die "Share directory ${SHARE} does not exist."

# ── Prepare test data ───────────────────────────────────────────────────────
TESTDIR_SHARE="${SHARE}/${BENCH_DIR}"
TESTDIR_MOUNT="${MOUNT}/${BENCH_DIR}"
SEQFILE="seqfile"

if [[ -f "${TESTDIR_SHARE}/${SEQFILE}" ]]; then
  info "Reusing existing test file ${TESTDIR_SHARE}/${SEQFILE}"
else
  info "Creating ${FILESIZE} test file on share..."
  mkdir -p "${TESTDIR_SHARE}"
  # Convert human size to dd count (supports G suffix)
  dd_count="$(echo "${FILESIZE}" | sed 's/G$//' | awk '{printf "%d", $1 * 1024}')"
  dd if=/dev/urandom of="${TESTDIR_SHARE}/${SEQFILE}" bs=1M count="${dd_count}" status=progress
  green "Test file created."
fi

if [[ ! -f "${TESTDIR_MOUNT}/${SEQFILE}" ]]; then
  die "Test file not visible through mount. Check that the share is exported."
fi
green "Test file visible through mount ($(du -h "${TESTDIR_MOUNT}/${SEQFILE}" | cut -f1))."

# Small files
existing_small=$(find "${TESTDIR_SHARE}" -maxdepth 1 -name 'small_*' ! -type d 2>/dev/null | wc -l)
if [[ "${existing_small}" -ge "${NRFILES}" ]]; then
  info "Reusing ${existing_small} existing small files."
else
  info "Creating ${NRFILES} small (4 KiB) files on share (named small_1, small_2, ...)..."
  for i in $(seq 1 "${NRFILES}"); do
    dd if=/dev/urandom of="${TESTDIR_SHARE}/small_${i}" bs=4k count=1 status=none
  done
  green "Small files created."
fi

mkdir -p "${OUTPUT}"

# ── fio runner ──────────────────────────────────────────────────────────────
run_fio_suite() {
  local suite_name="$1"
  local fio_file="$2"

  info "Running suite: ${suite_name} ..."

  local rendered="${OUTPUT}/${suite_name}.fio"
  sed \
    -e "s|\${DIRECTORY}|${TESTDIR_MOUNT}|g" \
    -e "s|\${FILENAME}|${SEQFILE}|g" \
    -e "s|\${SIZE}|${FILESIZE}|g" \
    -e "s|\${RUNTIME}|${RUNTIME}|g" \
    -e "s|\${NUMJOBS}|${NUMJOBS}|g" \
    -e "s|\${NRFILES}|${NRFILES}|g" \
    "${fio_file}" > "${rendered}"

  local json_out="${OUTPUT}/${suite_name}.json"

  fio "${rendered}" \
    --output-format=json \
    --output="${json_out}" \
    2>"${OUTPUT}/${suite_name}.stderr"

  if [[ ! -s "${json_out}" ]]; then
    warn "fio suite ${suite_name} produced no output."
    cat "${OUTPUT}/${suite_name}.stderr" 2>/dev/null
    return 1
  fi

  green "  ${suite_name} complete."
}

# Small-files suite needs special handling: fio cannot create files through
# FUSE so we must pass the existing small_*.dat filenames explicitly.
run_small_files_suite() {
  local suite_name="small-files"

  info "Running suite: ${suite_name} (shell-based benchmark) ..."

  # Collect existing small files
  local nfiles
  nfiles=$(find "${TESTDIR_SHARE}" -maxdepth 1 -name 'small_*' ! -type d 2>/dev/null | wc -l)
  if [[ "${nfiles}" -lt 1 ]]; then
    warn "No small files found in ${TESTDIR_SHARE}, skipping."
    return 1
  fi

  local json_out="${OUTPUT}/${suite_name}.json"
  local result_file="${OUTPUT}/${suite_name}.txt"

  # Shell-based benchmark: randomly read small files multiple rounds
  # Use dd for timing and collect throughput stats
  local total_bytes=0
  local total_ops=0
  local start_ns end_ns elapsed_ns
  local -a latencies_ns=()

  # Build array of small file paths through the mount
  local -a small_files=()
  while IFS= read -r f; do
    small_files+=("${TESTDIR_MOUNT}/$(basename "${f}")")
  done < <(find "${TESTDIR_SHARE}" -maxdepth 1 -name 'small_*' ! -type d -printf '%f\n' 2>/dev/null | sort)

  local nfiles_mount=${#small_files[@]}
  info "  Found ${nfiles_mount} small files through mount"

  # Read the test directory listing (readdir benchmark)
  local readdir_start readdir_end readdir_ms
  readdir_start=$(date +%s%N)
  ls "${TESTDIR_MOUNT}" >/dev/null 2>&1
  readdir_end=$(date +%s%N)
  readdir_ms=$(( (readdir_end - readdir_start) / 1000000 ))

  # Stat each file (getattr benchmark)
  local stat_total_ns=0
  local f
  for f in "${small_files[@]}"; do
    if stat "${f}" >/dev/null 2>&1; then
      : # success
    fi
  done
  local stat_start stat_end
  stat_start=$(date +%s%N)
  for f in "${small_files[@]}"; do
    stat "${f}" >/dev/null 2>&1
  done
  stat_end=$(date +%s%N)
  local stat_total_ns=$(( stat_end - stat_start ))
  local stat_avg_ns=$(( stat_total_ns / nfiles_mount ))

  # Random read benchmark: read random small files for the specified runtime
  start_ns=$(date +%s%N)
  local deadline=$(( start_ns / 1000000 + RUNTIME * 1000 ))
  local iter=0
  while true; do
    local now_ms
    now_ms=$(date +%s%N)
    now_ms=$(( now_ms / 1000000 ))
    [[ ${now_ms} -ge ${deadline} ]] && break

    # Pick a random file
    local idx=$(( RANDOM % nfiles_mount ))
    local file="${small_files[${idx}]}"

    # Time a single 4K read
    local read_start read_end
    read_start=$(date +%s%N)
    dd if="${file}" of=/dev/null bs=4k count=1 2>/dev/null
    read_end=$(date +%s%N)

    latencies_ns+=( $(( read_end - read_start )) )
    total_bytes=$(( total_bytes + 4096 ))
    total_ops=$(( total_ops + 1 ))
    iter=$(( iter + 1 ))
  done
  end_ns=$(date +%s%N)
  elapsed_ns=$(( end_ns - start_ns ))

  # Compute stats
  local elapsed_s
  elapsed_s=$(awk "BEGIN {printf \"%.3f\", ${elapsed_ns}/1000000000}")
  local bw_bytes=$(( total_bytes > 0 ? total_bytes * 1000000000 / elapsed_ns : 0 ))
  local iops
  iops=$(awk "BEGIN {printf \"%.1f\", ${total_ops} / ${elapsed_s}}")

  # Compute latency percentiles from latencies array
  local sorted_lat
  printf '%s\n' "${latencies_ns[@]}" | sort -n > "${OUTPUT}/${suite_name}.latencies"
  local nlat=${#latencies_ns[@]}
  local p50_idx=$(( nlat * 50 / 100 ))
  local p99_idx=$(( nlat * 99 / 100 ))
  local p50_lat p99_lat
  p50_lat=$(sed -n "${p50_idx}p" "${OUTPUT}/${suite_name}.latencies" 2>/dev/null || echo 0)
  p99_lat=$(sed -n "${p99_idx}p" "${OUTPUT}/${suite_name}.latencies" 2>/dev/null || echo 0)

  # Generate JSON output compatible with the summary parser
  cat > "${json_out}" <<JSONEOF
{
  "jobs": [{
    "jobname": "small-file-read",
    "job options": {
      "rw": "randread",
      "bs": "4k",
      "numjobs": "${NUMJOBS}",
      "runtime": "${RUNTIME}"
    },
    "read": {
      "bw_bytes": ${bw_bytes},
      "iops": ${iops},
      "total_ios": ${total_ops},
      "clat_ns": {
        "percentile": {
          "50.000000": ${p50_lat},
          "99.000000": ${p99_lat}
        }
      }
    },
    "extra": {
      "readdir_ms": ${readdir_ms},
      "stat_full_run_ms": $(( stat_total_ns / 1000000 )),
      "stat_avg_us": $(( stat_avg_ns / 1000 )),
      "files_tested": ${nfiles_mount}
    }
  }]
}
JSONEOF

  # Human-readable BW and latency formatting
  local bw_hr p50_hr p99_hr
  if (( bw_bytes >= 1073741824 )); then
    bw_hr="$(awk "BEGIN {printf \"%.1f GiB/s\", ${bw_bytes}/1073741824}" )"
  elif (( bw_bytes >= 1048576 )); then
    bw_hr="$(awk "BEGIN {printf \"%.1f MiB/s\", ${bw_bytes}/1048576}" )"
  elif (( bw_bytes >= 1024 )); then
    bw_hr="$(awk "BEGIN {printf \"%.1f KiB/s\", ${bw_bytes}/1024}" )"
  else
    bw_hr="${bw_bytes} B/s"
  fi

  if (( p50_lat >= 1000000000 )); then
    p50_hr="$(awk "BEGIN {printf \"%.1f s\", ${p50_lat}/1000000000}" )"
  elif (( p50_lat >= 1000000 )); then
    p50_hr="$(awk "BEGIN {printf \"%.1f ms\", ${p50_lat}/1000000}" )"
  elif (( p50_lat >= 1000 )); then
    p50_hr="$(awk "BEGIN {printf \"%.1f µs\", ${p50_lat}/1000}" )"
  else
    p50_hr="${p50_lat} ns"
  fi

  if (( p99_lat >= 1000000000 )); then
    p99_hr="$(awk "BEGIN {printf \"%.1f s\", ${p99_lat}/1000000000}" )"
  elif (( p99_lat >= 1000000 )); then
    p99_hr="$(awk "BEGIN {printf \"%.1f ms\", ${p99_lat}/1000000}" )"
  elif (( p99_lat >= 1000 )); then
    p99_hr="$(awk "BEGIN {printf \"%.1f µs\", ${p99_lat}/1000}" )"
  else
    p99_hr="${p99_lat} ns"
  fi

  # Human-readable output
  cat > "${result_file}" <<TXTEOF
small-file-read: (shell-based benchmark)
  Files tested:  ${nfiles_mount}
  Total ops:      ${total_ops}
  Elapsed:        ${elapsed_s}s
  BW:             ${bw_hr}
  IOPS:           ${iops}
  LAT p50:        ${p50_hr}
  LAT p99:        ${p99_hr}
  ---
  readdir:        ${readdir_ms} ms
  stat all files: $(( stat_total_ns / 1000000 )) ms total, $(( stat_avg_ns / 1000 )) µs avg
TXTEOF

  green "  ${suite_name} complete."
}

# ── Which suites to run ─────────────────────────────────────────────────────
ALL_SUITES="sequential random multi rand-multi small-files"
if [[ -n "${ONLY}" ]]; then
  # Split comma-separated list into space-separated
  SUITES="${ONLY//,/ }"
else
  SUITES="${ALL_SUITES}"
fi

declare -A SUITE_FILES
SUITE_FILES[sequential]="${FIO_DIR}/read-sequential.fio"
SUITE_FILES[random]="${FIO_DIR}/read-random.fio"
SUITE_FILES[multi]="${FIO_DIR}/read-multi.fio"
SUITE_FILES[rand-multi]="${FIO_DIR}/read-random-multi.fio"
SUITE_FILES[small-files]="${FIO_DIR}/read-small-files.fio"

for suite in ${SUITES}; do
  fio_file="${SUITE_FILES[${suite}]}"
  if [[ -z "${fio_file}" ]]; then
    warn "Unknown suite: ${suite}"
    continue
  fi
  if [[ ! -f "${fio_file}" ]]; then
    warn "Suite ${suite}: fio file ${fio_file} not found, skipping."
    continue
  fi
  if [[ "${suite}" == "small-files" ]]; then
    run_small_files_suite "${fio_file}"
  else
    run_fio_suite "${suite}" "${fio_file}"
  fi
done

# ── Summary ─────────────────────────────────────────────────────────────────
info "Generating summary..."

SUMMARY="${OUTPUT}/summary.txt"

format_bw() {
  local bytes="$1"
  if (( bytes >= 1073741824 )); then
    awk "BEGIN {printf \"%.1f GiB/s\", ${bytes}/1073741824}"
  elif (( bytes >= 1048576 )); then
    awk "BEGIN {printf \"%.1f MiB/s\", ${bytes}/1048576}"
  elif (( bytes >= 1024 )); then
    awk "BEGIN {printf \"%.1f KiB/s\", ${bytes}/1024}"
  else
    echo "${bytes} B/s"
  fi
}

format_lat() {
  local ns="$1"
  if (( ns >= 1000000000 )); then
    awk "BEGIN {printf \"%.1f s\", ${ns}/1000000000}"
  elif (( ns >= 1000000 )); then
    awk "BEGIN {printf \"%.1f ms\", ${ns}/1000000}"
  elif (( ns >= 1000 )); then
    awk "BEGIN {printf \"%.1f µs\", ${ns}/1000}"
  else
    echo "${ns} ns"
  fi
}

{
  echo "=========================================="
  echo "  Rift Read-Only Benchmark Summary"
  echo "  Date:    $(date)"
  echo "  Mount:   ${MOUNT}"
  echo "  Share:   ${SHARE}"
  echo "  Runtime: ${RUNTIME}s per test"
  echo "=========================================="
  echo ""
  printf "%-22s %-8s %-11s %-12s %-12s %-12s %-12s\n" \
    "JOB" "BLK" "PATTERN" "BW" "IOPS" "LAT p50" "LAT p99"
  printf "%-22s %-8s %-11s %-12s %-12s %-12s %-12s\n" \
    "--------------------" "------" "---------" "--------" "--------" "--------" "--------"

  for suite in ${SUITES}; do
    json="${OUTPUT}/${suite}.json"
    [[ -f "${json}" ]] || continue

    njobs=$(jq '.jobs | length' "${json}" 2>/dev/null || echo 0)
    for ((i=0; i<njobs; i++)); do
      name=$(jq -r ".jobs[${i}].jobname // \"?\"" "${json}" 2>/dev/null)
      rw=$(jq -r ".jobs[${i}][\"job options\"].rw // \"?\"" "${json}" 2>/dev/null)

      # group_reporting jobs: aggregate in single entry
      bs=$(jq -r ".jobs[${i}][\"job options\"].bs // \"?\"" "${json}" 2>/dev/null)
      bw_bytes=$(jq ".jobs[${i}].read.bw_bytes // 0" "${json}" 2>/dev/null)
      iops=$(jq -r ".jobs[${i}].read.iops // 0" "${json}" 2>/dev/null)

      p50=$(jq -r ".jobs[${i}].read.clat_ns.percentile.\"50.000000\" // 0" "${json}" 2>/dev/null)
      p99=$(jq -r ".jobs[${i}].read.clat_ns.percentile.\"99.000000\" // 0" "${json}" 2>/dev/null)

      # Handle group_reporting: bw_bytes/iops may be per-job or total
      njobs_entry=$(jq ".jobs[${i}][\"job options\"].numjobs // 1" "${json}" 2>/dev/null)

      printf "%-22s %-8s %-11s %-12s %-12s %-12s %-12s\n" \
        "${name}" "${bs}" "${rw}" \
        "$(format_bw "${bw_bytes}")" \
        "$(awk "BEGIN {printf \"%.1f\", ${iops}}")" \
        "$(format_lat "${p50}")" \
        "$(format_lat "${p99}")"
    done
  done

  echo ""
  echo "Full JSON results in: ${OUTPUT}/"
} | tee "${SUMMARY}"

# ── Save benchmark metadata ─────────────────────────────────────────────────
cat > "${OUTPUT}/meta.json" <<METAEOF
{
  "timestamp": "${TIMESTAMP}",
  "mount": "${MOUNT}",
  "share": "${SHARE}",
  "runtime_per_test": ${RUNTIME},
  "filesize": "${FILESIZE}",
  "numjobs": ${NUMJOBS},
  "small_files": ${NRFILES},
  "suites": "${SUITES}",
  "fio_version": "$(fio --version 2>&1 | head -1)",
  "kernel": "$(uname -r)",
  " rift_server_pid": "$(pgrep -f 'rift-server' || echo 'unknown')"
}
METAEOF

echo ""
bold "Done. Results saved to ${OUTPUT}/"
echo "  Summary:   ${SUMMARY}"
echo "  Metadata:  ${OUTPUT}/meta.json"
echo ""

# ── Cleanup ─────────────────────────────────────────────────────────────────
if [[ "${KEEP_DATA}" != true ]]; then
  info "Removing test data from share (use --keep-data to preserve)..."
  rm -rf "${TESTDIR_SHARE}"
  green "Test data removed."
else
  info "Test data preserved at ${TESTDIR_SHARE}"
fi