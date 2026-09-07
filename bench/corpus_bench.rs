//! Standalone native corpus benchmark. Build with:
//! cargo build --release --manifest-path bench/Cargo.toml

#[cfg(not(target_os = "linux"))]
compile_error!("corpus_bench currently requires Linux for RSS and address-space limits");

// Compile only the native engine modules, not the Python library. This keeps
// benchmark dependencies and entry points out of the production crate.
mod cache;
#[allow(dead_code)]
#[path = "../src/clock.rs"]
mod clock;
#[allow(dead_code)]
#[path = "../src/graph.rs"]
mod graph;
mod native;
#[allow(dead_code)]
#[path = "../src/partition.rs"]
mod partition;
#[allow(dead_code)]
#[path = "../src/prelude.rs"]
mod prelude;
#[allow(dead_code)]
#[path = "../src/values.rs"]
mod values;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

type Counts = BTreeMap<String, usize>;

const HELP: &str = "Usage: corpus_bench DIR [top_n] [OPTIONS]
Compare native whole-grid and partitioned row evaluation; JSON-encode every row.

  top_n               Largest files for memory measurements (default: 25)
  --check             Compare values with Formualizer and inspect Excel formula caches
  --results FILE      Create a new JSONL report; flush each result
  --timeout SECONDS   Worker deadline (default: 120)
  --memory-mb MIB     Worker address-space cap (default: 4096; 0 disables)
  --min-formulas N    Minimum formula count for splitting (default: 0)
  --jobs N            Concurrent file workers (default: available CPUs)
  -h, --help          Show this help

Linux only. Timings exclude file I/O and eligibility planning, not evaluator setup.
Every result row records the jobs and timeout it ran under. Native results are
not Python API timings.";

struct Args {
    root: PathBuf,
    top_n: usize,
    check: bool,
    results: Option<PathBuf>,
    timeout: Duration,
    memory_mb: u64,
    min_formulas: usize,
    jobs: usize,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        root: PathBuf::new(),
        top_n: 25,
        check: false,
        results: None,
        timeout: Duration::from_secs(120),
        memory_mb: 4096,
        min_formulas: 0,
        jobs: thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
    };
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => out.check = true,
            "--" => {
                positional.extend(args.cloned());
                break;
            }
            "--results" | "--timeout" | "--memory-mb" | "--min-formulas" | "--jobs" => {
                let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
                let invalid = || format!("invalid {arg}: {value}");
                match arg.as_str() {
                    "--results" => out.results = Some(value.into()),
                    "--timeout" => {
                        let secs: f64 = value.parse().map_err(|_| invalid())?;
                        out.timeout = Duration::try_from_secs_f64(secs).map_err(|_| invalid())?;
                        if out.timeout.is_zero() {
                            return Err(invalid());
                        }
                    }
                    "--memory-mb" => {
                        out.memory_mb = value.parse::<u64>().map_err(|_| invalid())?;
                        out.memory_mb.checked_mul(1024 * 1024).ok_or_else(invalid)?;
                    }
                    "--min-formulas" => out.min_formulas = value.parse().map_err(|_| invalid())?,
                    "--jobs" => {
                        out.jobs = value.parse().map_err(|_| invalid())?;
                        if out.jobs == 0 {
                            return Err(invalid());
                        }
                    }
                    _ => unreachable!(),
                }
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        return Err(HELP.into());
    }
    out.root = positional[0].clone().into();
    if let Some(n) = positional.get(1) {
        out.top_n = n.parse().map_err(|_| "top_n must be nonnegative")?;
    }
    Ok(out)
}

fn limits(memory_mb: u64) -> Result<(), String> {
    fn set(resource: libc::__rlimit_resource_t, cap: u64) -> Result<(), String> {
        let limit = libc::rlimit {
            rlim_cur: cap as libc::rlim_t,
            rlim_max: cap as libc::rlim_t,
        };
        // SAFETY: limit points to an initialized rlimit for this process only.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    set(libc::RLIMIT_CORE, 0)?;
    if memory_mb > 0 {
        set(
            libc::RLIMIT_AS,
            memory_mb
                .checked_mul(1024 * 1024)
                .ok_or("memory limit overflow")?,
        )?;
    }
    Ok(())
}

fn peak_mb() -> Result<f64, String> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes usage on success; no read occurs on failure.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { usage.assume_init() }.ru_maxrss as f64 / 1024.0)
}

// Drain pipes while the worker runs so diagnostics cannot deadlock the child.
// Retain only a bounded tail, including after an allocation failure or panic.
fn tail(mut pipe: impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let n = pipe.read(&mut buffer)?;
        if n == 0 {
            return Ok(kept);
        }
        if kept.len() + n > cap {
            kept.drain(..kept.len() + n - cap);
        }
        kept.extend_from_slice(&buffer[..n]);
    }
}

fn error(reason: impl ToString) -> Value {
    json!({"status": "error", "reason": reason.to_string()})
}

fn run_child(command: &mut Command, timeout: Duration) -> Value {
    let start = Instant::now();
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return error(e),
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_reader = thread::spawn(move || tail(stdout, 1024 * 1024));
    let err_reader = thread::spawn(move || tail(stderr, 64 * 1024));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() < timeout => {
                thread::sleep(
                    Duration::from_millis(5).min(timeout.saturating_sub(start.elapsed())),
                );
            }
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                break child.wait();
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e);
            }
        }
    };
    let out = out_reader.join().unwrap().unwrap_or_default();
    let err = err_reader.join().unwrap().unwrap_or_default();
    if timed_out {
        return json!({"status": "timeout", "reason": format!("exceeded {} seconds", timeout.as_secs_f64())});
    }
    let status = match status {
        Ok(s) => s,
        Err(e) => return error(e),
    };
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        let reason = String::from_utf8_lossy(&err)
            .trim()
            .chars()
            .rev()
            .take(2000)
            .collect::<String>();
        return json!({"status": "error", "returncode": status.code().or_else(|| status.signal().map(|s| -s)),
            "reason": if reason.is_empty() { format!("worker {status}") } else { reason.chars().rev().collect::<String>() }});
    }
    match serde_json::from_slice::<Value>(&out) {
        Ok(v) if v.is_object() && v["status"].is_string() => v,
        _ => json!({"status": "error", "reason": "invalid worker output",
            "output": String::from_utf8_lossy(&out).chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>()}),
    }
}

fn measure(exe: &Path, path: &Path, mode: &str, args: &Args) -> Value {
    let mut result = run_child(
        Command::new(exe)
            .arg("--worker")
            .arg(mode)
            .arg(path)
            .arg(args.memory_mb.to_string())
            .arg(args.min_formulas.to_string()),
        args.timeout,
    );
    if matches!(result["status"].as_str(), Some("error" | "timeout")) && result["stage"].is_null() {
        result["stage"] = json!(mode);
    }
    result
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>, ignored: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if kind.is_dir() {
            collect(&path, files, ignored)?;
        }
        // Do not follow directory symlinks: a cycle must not hang selection.
        else if (kind.is_file() || kind.is_symlink() && path.is_file())
            && path.extension().is_some_and(|e| e == "xlsx")
        {
            if entry.file_name().to_string_lossy().contains("_answer") {
                ignored.push(path)
            } else {
                files.push(path)
            }
        }
    }
    Ok(())
}

// Bounded workers, completion-order reporting: one slow file cannot delay
// flushing results from every other worker. No thread per corpus file.
fn foreach<T: Send, F: Fn(&Path) -> T + Sync>(
    files: &[PathBuf],
    jobs: usize,
    f: F,
    mut record: impl FnMut(&Path, T) -> Result<(), String>,
) -> Result<(), String> {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::sync_channel(jobs.max(1));
    thread::scope(|scope| {
        for _ in 0..jobs.min(files.len()) {
            let (tx, next, f) = (tx.clone(), &next, &f);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(i) else { break };
                if tx.send((i, f(path))).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for (i, value) in rx {
            record(&files[i], value)?;
        }
        Ok(())
    })
}

fn record(
    report: &mut Option<File>,
    args: &Args,
    path: &Path,
    phase: &str,
    result: &Value,
) -> Result<(), String> {
    if let Some(report) = report {
        let mut row = result.clone();
        row["phase"] = json!(phase);
        row["path"] = json!(path.strip_prefix(&args.root).unwrap_or(path).to_string_lossy());
        row["engine"] = json!("rust");
        row["schema_version"] = json!(3);
        row["baseline"] = json!("formualizer whole-file");
        // A duration is only comparable to another taken at the same
        // concurrency, and the console line that states the run's settings does
        // not survive into this file. Concurrent workers compete for the
        // machine and inflate both sides together, so the ratio stays
        // comparable across runs while absolute seconds do not. The deadline is
        // wall clock and does not scale with load either, so a workbook near it
        // can pass alone and time out in a crowd. Record both settings on every
        // row so a later reader can tell which numbers may be compared.
        row["jobs"] = json!(args.jobs);
        row["timeout_seconds"] = json!(args.timeout.as_secs_f64());
        serde_json::to_writer(&mut *report, &row).map_err(|e| e.to_string())?;
        report
            .write_all(b"\n")
            .and_then(|_| report.flush())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn count(counts: &mut Counts, result: &Value) -> usize {
    let status = result["status"].as_str().unwrap_or("error");
    *counts.entry(status.into()).or_default() += 1;
    usize::from(matches!(status, "error" | "timeout" | "mismatch"))
}

fn number(result: &Value, key: &str) -> f64 {
    result[key].as_f64().unwrap_or(0.0)
}
fn ok(result: &Value) -> bool {
    result["status"] == "ok"
}
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_unstable_by(f64::total_cmp);
    (v[(v.len() - 1) / 2] + v[v.len() / 2]) / 2.0
}
fn pct(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_unstable_by(f64::total_cmp);
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn sweep(args: Args) -> Result<i32, String> {
    let mut files = Vec::new();
    let mut ignored = Vec::new();
    collect(&args.root, &mut files, &mut ignored)?;
    files.sort();
    ignored.sort();
    let mut report = args
        .results
        .as_ref()
        .map(|p| OpenOptions::new().write(true).create_new(true).open(p))
        .transpose()
        .map_err(|e| e.to_string())?;
    for path in &ignored {
        record(
            &mut report,
            &args,
            path,
            "selection",
            &json!({"status":"skipped", "reason":"_answer filename exclusion"}),
        )?;
    }
    println!(
        "{} workbooks under {}; {} excluded by filename",
        files.len(),
        args.root.display(),
        ignored.len()
    );
    println!(
        "Worker limits: {}s, {} MiB address space (0 = unlimited), {} jobs",
        args.timeout.as_secs_f64(),
        args.memory_mb,
        args.jobs
    );
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let baseline = run_child(
        Command::new(&exe).arg("--baseline"),
        Duration::from_secs(30),
    );
    if !ok(&baseline) {
        return Err(format!("baseline failed: {baseline}"));
    }
    let base = number(&baseline, "peak");
    println!("\nMEMORY  peak RSS, native process baseline {base:.1} MiB subtracted");
    println!("   extent formulas whole_MiB part_MiB memory_gain whole_s part_s time_ratio file");
    let mut memory_files: Vec<_> = files
        .iter()
        .map(|p| {
            fs::metadata(p)
                .map(|m| (m.len(), p.clone()))
                .map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect::<Result<_, _>>()?;
    memory_files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let memory_files: Vec<_> = memory_files
        .into_iter()
        .take(args.top_n)
        .map(|(_, p)| p)
        .collect();
    let mut failures = 0;
    let mut memory_counts = Counts::new();
    let mut gains = Vec::new();
    foreach(
        &memory_files,
        args.jobs,
        |path| {
            ["whole", "partitioned"].map(|mode| {
                let mut result = measure(&exe, path, mode, &args);
                if ok(&result) {
                    result["baseline_mb"] = json!(base);
                    result["net_peak_mb"] = json!(number(&result, "peak") - base);
                }
                result
            })
        },
        |path, [w, mut p]| {
            if ok(&w) && ok(&p) {
                let (wm, pm) = (number(&w, "net_peak_mb"), number(&p, "net_peak_mb"));
                p["vs_formualizer"] = json!({
                    "time_ratio":number(&p,"secs") / number(&w,"secs"),
                    "peak_ratio":number(&p,"peak") / number(&w,"peak"),
                    "peak_delta_mb":number(&p,"peak") - number(&w,"peak"),
                    "net_peak_ratio":if wm > 1.0 && pm > 1.0 { Some(pm / wm) } else { None },
                    "net_peak_delta_mb":pm - wm});
            }
            for (mode, result) in [("memory_whole", &w), ("memory_partitioned", &p)] {
                record(&mut report, &args, path, mode, result)?;
                failures += count(&mut memory_counts, result);
                if result["status"] == "error" || result["status"] == "timeout" {
                    eprintln!(
                        "{mode} {}: stage={} reason={}",
                        path.display(),
                        result["stage"],
                        result["reason"]
                    );
                }
            }
            if ok(&w) && ok(&p) {
                let (wm, pm) = (number(&w, "net_peak_mb"), number(&p, "net_peak_mb"));
                let gain = if wm > 1.0 && pm > 1.0 {
                    gains.push(wm / pm);
                    format!("{:.2}x", wm / pm)
                } else {
                    "n/a".into()
                };
                println!(
                    "   {} {} {wm:.1} {pm:.1} {gain} {:.4} {:.4} {:.2}x {}",
                    w["extent"],
                    w["formulas"],
                    number(&w, "secs"),
                    number(&p, "secs"),
                    number(&p, "secs") / number(&w, "secs"),
                    path.strip_prefix(&args.root).unwrap_or(path).display()
                );
            }
            std::io::stdout().flush().map_err(|e| e.to_string())
        },
    )?;
    if !gains.is_empty() {
        println!(
            "   median memory gain {:.2}x over {} workbooks",
            median(&gains),
            gains.len()
        );
    }
    println!("   memory worker outcomes: {memory_counts:?}");
    println!("\nSPEED  partitioned / whole (>1 means slower); native JSON-encode every row");
    let mut counts = Counts::new();
    let mut checks = Counts::new();
    let mut cache_checks = Counts::new();
    let mut cache_examples = 0;
    let mut rows = Vec::new();
    let mut completed = 0;
    foreach(
        &files,
        args.jobs,
        |path| {
            let mut speed = measure(&exe, path, "speed", &args);
            // A whole-side abort kills the combined worker before it prints,
            // so re-run the partitioned side alone rather than lose its data.
            if !ok(&speed) {
                let part = measure(&exe, path, "speed_partitioned", &args);
                if ok(&part) {
                    speed["partitioned"] = part["partitioned"].clone();
                    speed["partitioned_output"] = part["partitioned_output"].clone();
                    speed["partitioned_status"] = json!("ok");
                }
            }
            let part_available =
                ok(&speed) || speed.get("partitioned_status") == Some(&json!("ok"));
            let check = args.check.then(|| {
                if !part_available {
                    json!({"status":"skipped", "reason":format!("speed result: {}", speed["status"].as_str().unwrap_or("error")), "detail":speed["reason"]})
                } else {
                    let c = measure(&exe, path, "check", &args);
                    if matches!(c["status"].as_str(), Some("error" | "timeout")) {
                        // The whole baseline died (possibly aborting the worker
                        // with no output): salvage the partitioned-only evidence
                        // instead of reporting nothing.
                        let mut cp = measure(&exe, path, "check_part", &args);
                        cp["whole_status"] = c["status"].clone();
                        cp["whole_stage"] = c["stage"].clone();
                        cp["whole_reason"] = c["reason"].clone();
                        cp
                    } else {
                        c
                    }
                }
            });
            (speed, check)
        },
        |path, (speed, check)| {
            record(&mut report, &args, path, "speed", &speed)?;
            failures += count(&mut counts, &speed);
            if speed["status"] == "error" || speed["status"] == "timeout" {
                eprintln!(
                    "speed {}: stage={} reason={}",
                    path.display(),
                    speed["stage"],
                    speed["reason"]
                );
            }
            if ok(&speed) {
                rows.push(speed);
            }
            if let Some(check) = check {
                record(&mut report, &args, path, "correctness", &check)?;
                // A salvaged check still lost its whole baseline: that is a
                // failed measurement even when the partitioned evidence is ok.
                if check.get("whole_status").is_some() {
                    failures += 1;
                }
                failures += count(&mut checks, &check);
                if matches!(check["status"].as_str(), Some("error" | "timeout")) {
                    eprintln!(
                        "correctness {}: stage={} reason={}",
                        path.display(),
                        check["stage"],
                        check["reason"]
                    );
                } else if check["status"] == "mismatch" {
                    eprintln!(
                        "correctness {}: {} differences vs formualizer {} first={}",
                        path.display(),
                        check["differences"],
                        check["comparisons"]["materialized_vs_formualizer"]["kinds"],
                        check["first"]
                    );
                }
                for mode in ["formualizer", "materialized", "streamed"] {
                    let evidence = &check["excel_cached"][mode];
                    if let Some(status) = evidence["status"].as_str() {
                        *cache_checks.entry(format!("{mode}:{status}")).or_default() += 1;
                        if status == "mismatch" && mode == "formualizer" && cache_examples < 3 {
                            eprintln!(
                                "Excel cache warning {}: {}",
                                path.display(),
                                evidence["samples"][0]
                            );
                            cache_examples += 1;
                        }
                    }
                }
            }
            completed += 1;
            if completed % 100 == 0 || completed == files.len() {
                println!(
                    "   {completed}/{} speed={counts:?} correctness={checks:?}",
                    files.len()
                );
                std::io::stdout().flush().map_err(|e| e.to_string())?;
            }
            Ok(())
        },
    )?;
    println!(
        "\nSPEED SUMMARY  {} measured workbooks; outcomes={counts:?}",
        rows.len()
    );
    let ratios = |rows: &[Value]| {
        rows.iter()
            .filter(|r| number(r, "whole") > 0.0)
            .map(|r| number(r, "partitioned") / number(r, "whole"))
            .collect::<Vec<_>>()
    };
    let speed = ratios(&rows);
    if !speed.is_empty() {
        println!(
            "   median {:.2}x   p10 {:.2}x   p90 {:.2}x   worst {:.2}x",
            median(&speed),
            pct(&speed, 0.1),
            pct(&speed, 0.9),
            pct(&speed, 1.0)
        );
        // Each row holds the duration measured for one file, so the total is
        // their sum at any concurrency. Concurrent workers compete for the
        // machine and inflate the parts, so read this as the work the run did,
        // not as the time the same corpus would take on an idle machine.
        println!(
            "   total: whole {:.2}s   partitioned {:.2}s",
            rows.iter().map(|r| number(r, "whole")).sum::<f64>(),
            rows.iter().map(|r| number(r, "partitioned")).sum::<f64>()
        );
        rows.sort_by(|a, b| number(b, "extent").total_cmp(&number(a, "extent")));
        let big = ratios(&rows[..args.top_n.min(rows.len())]);
        if !big.is_empty() {
            println!(
                "   {} largest by extent: median {:.2}x",
                big.len(),
                median(&big)
            );
        }
        let extents: Vec<_> = rows.iter().map(|r| number(r, "extent")).collect();
        println!(
            "   extent cells: median {:.0}   p90 {:.0}   max {:.0}",
            pct(&extents, 0.5),
            pct(&extents, 0.9),
            pct(&extents, 1.0)
        );
    }
    if args.check {
        println!("CORRECTNESS  exact native comparison with Formualizer whole-file: {checks:?}");
        println!("EXCEL CACHE  formula-cache evidence (may be stale; not an exit-code gate): {cache_checks:?}");
    }
    println!(
        "Complete. {failures} failed measurements/checks. Results: {}",
        args.results
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not saved)".into())
    );
    Ok(i32::from(failures > 0))
}

fn main() {
    let args: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(|s| s.into_string().map_err(|_| "arguments must be UTF-8"))
        .collect()
    {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if args.first().is_some_and(|s| s == "--baseline") {
        let result = peak_mb()
            .map(|peak| json!({"status":"ok", "peak":peak}))
            .unwrap_or_else(error);
        println!("{result}");
        return;
    }
    if args.first().is_some_and(|s| s == "--worker") {
        let mut stage = "arguments";
        let mut run = || -> Result<Value, String> {
            if args.len() != 5 {
                return Err("invalid worker arguments".into());
            }
            let min_formulas = args[4].parse().map_err(|_| "invalid minimum formulas")?;
            stage = "resource_limits";
            limits(args[3].parse().map_err(|_| "invalid memory cap")?)?;
            stage = "file_read";
            let data = fs::read(&args[2]).map_err(|e| e.to_string())?;
            let mut result = native::worker(&args[1], &data, min_formulas).unwrap_or_else(
                |(stage, reason)| json!({"status":"error", "stage":stage, "reason":reason}),
            );
            if ok(&result) && matches!(args[1].as_str(), "whole" | "partitioned") {
                stage = "peak_rss";
                result["peak"] = json!(peak_mb()?);
            }
            Ok(result)
        };
        let result = run()
            .unwrap_or_else(|reason| json!({"status":"error", "stage":stage, "reason":reason}));
        println!("{result}");
        return;
    }
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return;
    }
    match parse(&args).and_then(sweep) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}
