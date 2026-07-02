//! Integration test for the syscall-summary recorder.
//!
//! Spawns a workload that makes many syscalls, traces it with
//! `--add-recorder syscall-summary`, and validates the `syscall_summary` table.
//!
//! Requires root/BPF privileges; run via:
//! ```
//! ./scripts/run-integration-tests.sh syscall_summary
//! ```

use systing::{systing, Config};
use tempfile::TempDir;

#[test]
#[ignore] // Requires root/BPF privileges
fn test_syscall_summary_e2e() {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let trace_path = dir.path().join("trace.pb");

    // Workload: a loop of short sleeps. Each iteration issues a blocking
    // sleep syscall with a measurable duration, so at least one (thread,
    // syscall) aggregate ends up with count/total_ns/max_ns all > 0.
    // spawn_traced_child holds the child until BPF is attached, so no
    // pre-sleep is needed.
    let py_prog = "import time\n\
                   for _ in range(200):\n\
                   \x20 time.sleep(0.005)\n";
    let run_cmd = vec!["python3".to_string(), "-c".to_string(), py_prog.to_string()];
    let traced_child =
        systing::traced_command::spawn_traced_child(&run_cmd).expect("Failed to spawn child");
    let child_pid = traced_child.pid as i32;

    eprintln!("Recording syscall-summary trace (pid {child_pid})...");

    let config = Config {
        syscall_summary: true,
        parquet_only: true,
        output_dir: dir.path().to_path_buf(),
        output: trace_path,
        ..Config::default()
    };

    let exit_code = systing(config, Some(traced_child)).expect("systing recording failed");
    assert_eq!(exit_code, 0, "sleep workload should exit with code 0");
    eprintln!("Recording complete.\n");

    // --- Check: syscall_summary parquet file exists ---
    assert!(
        dir.path().join("syscall_summary.parquet").exists(),
        "syscall_summary.parquet not found in output dir"
    );

    // --- Convert to DuckDB for SQL assertions ---
    let duckdb_path = dir.path().join("trace.duckdb");
    systing::duckdb::parquet_to_duckdb(dir.path(), &duckdb_path, "systest")
        .expect("DuckDB conversion failed");
    let conn = duckdb::Connection::open(&duckdb_path).expect("Failed to open DuckDB");

    // --- Check: the traced pid has at least one fully-populated aggregate ---
    // count > 0 (calls seen), total_ns > 0 (time accumulated from enter/exit
    // deltas), max_ns > 0 (a slowest call recorded).
    eprintln!("  syscall_summary populated for traced pid...");
    let valid_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM syscall_summary
             WHERE pid = ? AND count > 0 AND total_ns > 0 AND max_ns > 0",
            [child_pid],
            |row| row.get(0),
        )
        .expect("valid-rows query failed");
    assert!(
        valid_rows >= 1,
        "expected >= 1 syscall_summary row with count/total_ns/max_ns all > 0 \
         for pid {child_pid}, got {valid_rows}"
    );

    // --- Check: per-row invariants hold for the traced pid ---
    // syscall_name is resolved (non-empty), every aggregate saw at least one
    // call, and the summed time is never less than the single slowest call.
    eprintln!("  syscall_summary per-row invariants...");
    let bad_invariants: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM syscall_summary
             WHERE pid = ? AND (syscall_name = '' OR count < 1 OR total_ns < max_ns)",
            [child_pid],
            |row| row.get(0),
        )
        .expect("invariants query failed");
    assert_eq!(
        bad_invariants, 0,
        "every syscall_summary row must have a name, count >= 1, and total_ns >= max_ns"
    );

    // --- Check: rows join to a known thread (utid resolved consistently) ---
    eprintln!("  syscall_summary utid join integrity...");
    let orphan_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM syscall_summary ss
             WHERE NOT EXISTS (SELECT 1 FROM thread t WHERE t.utid = ss.utid)",
            [],
            |row| row.get(0),
        )
        .expect("orphan-rows query failed");
    assert_eq!(
        orphan_rows, 0,
        "every syscall_summary row should map to a known thread"
    );

    eprintln!("syscall-summary e2e checks passed.");
}
