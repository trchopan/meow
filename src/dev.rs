use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use uuid::Uuid;

use crate::cli::{BenchFlushArgs, DevSmokeArgs};

#[derive(Debug, Clone)]
struct ProbeSummary {
    total_messages: u64,
    relative_messages: u64,
    sum_abs_dx: u64,
    sum_abs_dy: u64,
    msg_rate_per_sec: f64,
    bytes_rate_per_sec: f64,
    max_inter_msg_gap_sec: f64,
    inter_msg_gap_ms_p50: f64,
    inter_msg_gap_ms_p95: f64,
    inter_msg_gap_ms_p99: f64,
}

#[derive(Debug, Clone)]
struct BenchRunResult {
    flush_tick_ms: u64,
    run_index: u64,
    summary: ProbeSummary,
}

pub(crate) async fn run_dev_smoke(args: DevSmokeArgs) -> Result<()> {
    if args.duration_secs == 0 {
        bail!("duration must be > 0");
    }

    let temp_dir = std::env::temp_dir().join(format!("meow-dev-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;

    println!("dev smoke state dir: {}", temp_dir.display());

    let mut host = match Command::new(std::env::current_exe()?)
        .arg("host")
        .env("MEOW_STATE_DIR", &temp_dir)
        .env("MEOW_DEV_SMOKE", "1")
        .env("MEOW_DEV_SIDE", side_arg(args.side))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(err).context("failed to spawn host for dev smoke");
        }
    };

    let host_stdout = match host.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = cleanup_dev_smoke(&temp_dir, &mut host, None);
            return Err(anyhow!("host stdout not available"));
        }
    };

    let (ready_tx, ready_rx) = mpsc::sync_channel::<(String, String)>(1);
    let mut host_reader = Some(thread::spawn(move || {
        let reader = BufReader::new(host_stdout);
        let mut endpoint_id = None;
        let mut secret = None;
        let mut sent_ready = false;
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    println!("[host] {line}");
                    if let Some(id) = line.strip_prefix("Host endpoint id: ") {
                        endpoint_id = Some(id.trim().to_string());
                    } else if let Some(s) = line.strip_prefix("Session secret: ") {
                        secret = Some(s.trim().to_string());
                    }
                    if !sent_ready && let (Some(id), Some(secret)) = (&endpoint_id, &secret) {
                        let _ = ready_tx.send((id.clone(), secret.clone()));
                        sent_ready = true;
                    }
                }
                Err(_) => break,
            }
        }
    }));

    let run_result = (|| -> Result<()> {
        let (host_id, secret) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("timed out waiting for host endpoint id and session secret")?;

        let attach_status = Command::new(std::env::current_exe()?)
            .arg("attach")
            .arg(&host_id)
            .arg(&secret)
            .arg("--side")
            .arg(side_arg(args.side))
            .arg("--probe-received")
            .arg("--probe-duration-secs")
            .arg(args.duration_secs.to_string())
            .arg("--no-inject")
            .env("MEOW_STATE_DIR", &temp_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run attach probe in dev smoke")?;

        if !attach_status.success() {
            bail!("dev smoke attach probe failed with status {attach_status}");
        }

        Ok(())
    })();

    let cleanup_result = cleanup_dev_smoke(&temp_dir, &mut host, host_reader.take());

    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => {}
        (Err(run_err), Ok(())) => return Err(run_err),
        (Ok(()), Err(cleanup_err)) => return Err(cleanup_err),
        (Err(run_err), Err(cleanup_err)) => {
            return Err(run_err.context(format!("cleanup failed: {cleanup_err:#}")));
        }
    }

    println!("dev smoke completed successfully");
    Ok(())
}

pub(crate) async fn run_bench_flush(args: BenchFlushArgs) -> Result<()> {
    if args.duration_secs == 0 {
        bail!("duration must be > 0");
    }
    if args.event_rate_hz == 0 {
        bail!("event-rate-hz must be > 0");
    }
    if args.runs == 0 {
        bail!("runs must be > 0");
    }
    if args.flush_a_ms == 0 || args.flush_b_ms == 0 {
        bail!("flush tick values must be > 0");
    }

    println!(
        "bench flush start: duration={}s side={} rate_hz={} runs={} flush_a_ms={} flush_b_ms={} dx={} dy={}",
        args.duration_secs,
        side_arg(args.side),
        args.event_rate_hz,
        args.runs,
        args.flush_a_ms,
        args.flush_b_ms,
        args.dx,
        args.dy
    );

    let mut results = Vec::<BenchRunResult>::new();
    for flush_tick_ms in [args.flush_a_ms, args.flush_b_ms] {
        for run_index in 1..=args.runs {
            println!(
                "bench run start: flush_tick_ms={} run={}/{}",
                flush_tick_ms, run_index, args.runs
            );
            let summary = run_single_bench_flush_case(&args, flush_tick_ms).with_context(|| {
                format!(
                    "bench run failed for flush_tick_ms={} run={}/{}",
                    flush_tick_ms, run_index, args.runs
                )
            })?;

            println!(
                "bench run result: flush_tick_ms={} run={} total_messages={} relative_messages={} msg_rate_per_sec={:.2} bytes_rate_per_sec={:.2} max_inter_msg_gap_ms={:.3} p50_gap_ms={:.3} p95_gap_ms={:.3} p99_gap_ms={:.3}",
                flush_tick_ms,
                run_index,
                summary.total_messages,
                summary.relative_messages,
                summary.msg_rate_per_sec,
                summary.bytes_rate_per_sec,
                summary.max_inter_msg_gap_sec * 1000.0,
                summary.inter_msg_gap_ms_p50,
                summary.inter_msg_gap_ms_p95,
                summary.inter_msg_gap_ms_p99,
            );

            results.push(BenchRunResult {
                flush_tick_ms,
                run_index,
                summary,
            });
        }
    }

    print_bench_aggregate(&results, args.flush_a_ms, args.flush_b_ms);
    println!("bench flush completed");
    Ok(())
}

fn run_single_bench_flush_case(args: &BenchFlushArgs, flush_tick_ms: u64) -> Result<ProbeSummary> {
    let temp_dir = std::env::temp_dir().join(format!("meow-bench-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;

    println!("bench state dir: {}", temp_dir.display());

    let mut host = match Command::new(std::env::current_exe()?)
        .arg("host")
        .env("MEOW_STATE_DIR", &temp_dir)
        .env("MEOW_BENCH_FLUSH", "1")
        .env("MEOW_BENCH_SIDE", side_arg(args.side))
        .env("MEOW_BENCH_EVENT_RATE_HZ", args.event_rate_hz.to_string())
        .env("MEOW_BENCH_DX", args.dx.to_string())
        .env("MEOW_BENCH_DY", args.dy.to_string())
        .env("MEOW_FLUSH_TICK_MS", flush_tick_ms.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(err).context("failed to spawn host for flush benchmark");
        }
    };

    let host_stdout = match host.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = cleanup_dev_smoke(&temp_dir, &mut host, None);
            return Err(anyhow!("host stdout not available"));
        }
    };

    let (ready_tx, ready_rx) = mpsc::sync_channel::<(String, String)>(1);
    let mut host_reader = Some(thread::spawn(move || {
        let reader = BufReader::new(host_stdout);
        let mut endpoint_id = None;
        let mut secret = None;
        let mut sent_ready = false;
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    println!("[host] {line}");
                    if let Some(id) = line.strip_prefix("Host endpoint id: ") {
                        endpoint_id = Some(id.trim().to_string());
                    } else if let Some(s) = line.strip_prefix("Session secret: ") {
                        secret = Some(s.trim().to_string());
                    }
                    if !sent_ready && let (Some(id), Some(secret)) = (&endpoint_id, &secret) {
                        let _ = ready_tx.send((id.clone(), secret.clone()));
                        sent_ready = true;
                    }
                }
                Err(_) => break,
            }
        }
    }));

    let run_result = (|| -> Result<ProbeSummary> {
        let (host_id, secret) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("timed out waiting for host endpoint id and session secret")?;

        let attach_output = Command::new(std::env::current_exe()?)
            .arg("attach")
            .arg(&host_id)
            .arg(&secret)
            .arg("--side")
            .arg(side_arg(args.side))
            .arg("--probe-received")
            .arg("--probe-summary-only")
            .arg("--probe-duration-secs")
            .arg(args.duration_secs.to_string())
            .arg("--no-inject")
            .env("MEOW_STATE_DIR", &temp_dir)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .context("failed to run attach probe in flush benchmark")?;

        let attach_stdout = String::from_utf8_lossy(&attach_output.stdout);
        for line in attach_stdout.lines() {
            println!("[attach] {line}");
        }

        if !attach_output.status.success() {
            bail!(
                "flush benchmark attach probe failed with status {}",
                attach_output.status
            );
        }

        parse_probe_summary(&attach_stdout)
    })();

    let cleanup_result = cleanup_dev_smoke(&temp_dir, &mut host, host_reader.take());

    match (run_result, cleanup_result) {
        (Ok(summary), Ok(())) => Ok(summary),
        (Err(run_err), Ok(())) => Err(run_err),
        (Ok(_), Err(cleanup_err)) => Err(cleanup_err),
        (Err(run_err), Err(cleanup_err)) => {
            Err(run_err.context(format!("cleanup failed: {cleanup_err:#}")))
        }
    }
}

fn parse_probe_summary(output: &str) -> Result<ProbeSummary> {
    let mut summary = ProbeSummary {
        total_messages: 0,
        relative_messages: 0,
        sum_abs_dx: 0,
        sum_abs_dy: 0,
        msg_rate_per_sec: 0.0,
        bytes_rate_per_sec: 0.0,
        max_inter_msg_gap_sec: 0.0,
        inter_msg_gap_ms_p50: 0.0,
        inter_msg_gap_ms_p95: 0.0,
        inter_msg_gap_ms_p99: 0.0,
    };

    let mut saw_summary_header = false;
    for line in output.lines() {
        if line.trim() == "client probe summary:" {
            saw_summary_header = true;
            continue;
        }
        if !saw_summary_header {
            continue;
        }
        for token in line.split_whitespace() {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            match key {
                "total_messages" => summary.total_messages = parse_u64(value)?,
                "relative_messages" => summary.relative_messages = parse_u64(value)?,
                "sum_abs_dx" => summary.sum_abs_dx = parse_u64(value)?,
                "sum_abs_dy" => summary.sum_abs_dy = parse_u64(value)?,
                "msg_rate_per_sec" => summary.msg_rate_per_sec = parse_f64(value)?,
                "bytes_rate_per_sec" => summary.bytes_rate_per_sec = parse_f64(value)?,
                "max_inter_msg_gap" => summary.max_inter_msg_gap_sec = parse_f64(value)?,
                "inter_msg_gap_ms_p50" => summary.inter_msg_gap_ms_p50 = parse_f64(value)?,
                "inter_msg_gap_ms_p95" => summary.inter_msg_gap_ms_p95 = parse_f64(value)?,
                "inter_msg_gap_ms_p99" => summary.inter_msg_gap_ms_p99 = parse_f64(value)?,
                _ => {}
            }
        }
    }

    if !saw_summary_header {
        bail!("client probe summary not found in attach output");
    }
    Ok(summary)
}

fn parse_u64(value: &str) -> Result<u64> {
    value
        .trim_end_matches([',', ')'])
        .parse::<u64>()
        .with_context(|| format!("invalid u64 metric value {value:?}"))
}

fn parse_f64(value: &str) -> Result<f64> {
    value
        .trim_end_matches(['s', ',', ')'])
        .parse::<f64>()
        .with_context(|| format!("invalid f64 metric value {value:?}"))
}

fn print_bench_aggregate(results: &[BenchRunResult], flush_a_ms: u64, flush_b_ms: u64) {
    fn median(values: &mut [f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    }

    fn group_metric(
        results: &[BenchRunResult],
        flush_tick_ms: u64,
        metric: fn(&ProbeSummary) -> f64,
    ) -> f64 {
        let mut values = results
            .iter()
            .filter(|r| r.flush_tick_ms == flush_tick_ms)
            .map(|r| metric(&r.summary))
            .collect::<Vec<_>>();
        median(&mut values)
    }

    let a_msg_rate = group_metric(results, flush_a_ms, |s| s.msg_rate_per_sec);
    let b_msg_rate = group_metric(results, flush_b_ms, |s| s.msg_rate_per_sec);
    let a_p95 = group_metric(results, flush_a_ms, |s| s.inter_msg_gap_ms_p95);
    let b_p95 = group_metric(results, flush_b_ms, |s| s.inter_msg_gap_ms_p95);
    let a_p99 = group_metric(results, flush_a_ms, |s| s.inter_msg_gap_ms_p99);
    let b_p99 = group_metric(results, flush_b_ms, |s| s.inter_msg_gap_ms_p99);
    let a_max_ms = group_metric(results, flush_a_ms, |s| s.max_inter_msg_gap_sec * 1000.0);
    let b_max_ms = group_metric(results, flush_b_ms, |s| s.max_inter_msg_gap_sec * 1000.0);
    let p95_delta_pct = percent_change(a_p95, b_p95);
    let p99_delta_pct = percent_change(a_p99, b_p99);
    let max_gap_delta_pct = percent_change(a_max_ms, b_max_ms);

    println!("bench aggregate medians:");
    println!(
        "  flush_tick_ms={} msg_rate_per_sec={:.2} p95_gap_ms={:.3} p99_gap_ms={:.3} max_gap_ms={:.3}",
        flush_a_ms, a_msg_rate, a_p95, a_p99, a_max_ms
    );
    println!(
        "  flush_tick_ms={} msg_rate_per_sec={:.2} p95_gap_ms={:.3} p99_gap_ms={:.3} max_gap_ms={:.3}",
        flush_b_ms, b_msg_rate, b_p95, b_p99, b_max_ms
    );
    println!(
        "  delta flush {}->{}: p95_gap={:.2}% p99_gap={:.2}% max_gap={:.2}%",
        flush_a_ms, flush_b_ms, p95_delta_pct, p99_delta_pct, max_gap_delta_pct
    );

    for result in results {
        println!(
            "bench record: flush_tick_ms={} run={} msg_rate_per_sec={:.2} p95_gap_ms={:.3} p99_gap_ms={:.3} max_gap_ms={:.3}",
            result.flush_tick_ms,
            result.run_index,
            result.summary.msg_rate_per_sec,
            result.summary.inter_msg_gap_ms_p95,
            result.summary.inter_msg_gap_ms_p99,
            result.summary.max_inter_msg_gap_sec * 1000.0
        );
    }
}

fn percent_change(base: f64, next: f64) -> f64 {
    if base.abs() < f64::EPSILON {
        return 0.0;
    }
    ((next - base) / base) * 100.0
}

fn side_arg(side: crate::model::Side) -> &'static str {
    match side {
        crate::model::Side::Left => "left",
        crate::model::Side::Right => "right",
        crate::model::Side::Up => "up",
        crate::model::Side::Down => "down",
    }
}

fn stop_host_process(state_dir: &Path, host: &mut std::process::Child) -> Result<()> {
    let stop_status = Command::new(std::env::current_exe()?)
        .arg("stop")
        .env("MEOW_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if let Ok(status) = stop_status
        && status.success()
    {
        let _ = host.wait();
        return Ok(());
    }

    let _ = host.kill();
    let _ = host.wait();
    Ok(())
}

fn cleanup_dev_smoke(
    state_dir: &Path,
    host: &mut std::process::Child,
    host_reader: Option<std::thread::JoinHandle<()>>,
) -> Result<()> {
    stop_host_process(state_dir, host).context("failed to stop dev smoke host")?;

    if let Some(reader) = host_reader {
        reader
            .join()
            .map_err(|_| anyhow!("host output reader thread panicked"))?;
    }

    match std::fs::remove_dir_all(state_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to remove dev smoke state dir {}",
                state_dir.display()
            )
        }),
    }
}
