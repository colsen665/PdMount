//! `pdmount stats` and the Prometheus textfile (DESIGN.md §5.8): counters,
//! gauges and latency summaries, from the daemon's `Metrics`.

use crate::control::{self, LatencyReply, PoolClassReply, Request, StatsReply};
use pdmount_core::ProfilePaths;
use pdmount_core::engine::Engine;
use pdmount_core::units::ByteSize;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const TEXTFILE_INTERVAL: Duration = Duration::from_secs(15);

/// The daemon's answer to `Stats`; with `reset`, the latency histograms
/// are cleared after they were read.
pub fn reply(engine: &Engine, reset: bool) -> StatsReply {
    let metrics = engine.metrics();
    let latency = metrics
        .snapshot_latency()
        .into_iter()
        .map(|(name, summary)| LatencyReply {
            name: name.to_owned(),
            count: summary.count,
            p50_us: summary.p50,
            p90_us: summary.p90,
            p99_us: summary.p99,
            max_us: summary.max,
        })
        .collect();
    if reset {
        metrics.reset_latency();
    }
    StatsReply {
        counters: metrics
            .snapshot()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
        pool: pool_reply(engine),
        gauges: engine
            .gauges()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
        latency,
    }
}

/// The pool slots per class.
pub fn pool_reply(engine: &Engine) -> Vec<PoolClassReply> {
    engine
        .cache_pool_usage()
        .into_iter()
        .map(|class| PoolClassReply {
            class: if class.scratch {
                "scratch".to_owned()
            } else {
                ByteSize(class.capacity as u64).to_string()
            },
            slots: class.slots,
            in_use: class.in_use,
        })
        .collect()
}

/// `pdmount stats [--json] [--reset]`.
pub async fn run(paths: &ProfilePaths, json: bool, reset: bool) -> anyhow::Result<ExitCode> {
    let Some(reply) = control::call_ok::<StatsReply>(
        &paths.control_socket(),
        Request::Stats { reset },
        CONTROL_TIMEOUT,
    )
    .await?
    else {
        eprintln!("Profile {} is not mounted.", paths.profile);
        return Ok(ExitCode::FAILURE);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
    } else {
        print!("{}", text(&reply));
    }
    Ok(ExitCode::SUCCESS)
}

/// Counters, pool slots and gauges as name/value lines, then the latency
/// table in µs.
fn text(reply: &StatsReply) -> String {
    let mut rows: Vec<(String, String)> = reply
        .counters
        .iter()
        .map(|(name, value)| (name.clone(), value.to_string()))
        .collect();
    for class in &reply.pool {
        rows.push((
            format!("cache_pool_slots_{}", class.class),
            format!("{} of {} in use", class.in_use, class.slots),
        ));
    }
    rows.extend(
        reply
            .gauges
            .iter()
            .map(|(name, value)| (name.clone(), value.to_string())),
    );
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, value) in rows {
        let _ = writeln!(out, "{name:width$}  {value}");
    }
    if reply.latency.is_empty() {
        return out;
    }
    let width = reply
        .latency
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or(0)
        .max("latency (µs)".chars().count());
    let _ = writeln!(
        out,
        "\n{:width$}  {:>9}  {:>9}  {:>9}  {:>9}",
        "latency (µs)", "count", "p50", "p99", "max"
    );
    for row in &reply.latency {
        let _ = writeln!(
            out,
            "{:width$}  {:>9}  {:>9}  {:>9}  {:>9}",
            row.name, row.count, row.p50_us, row.p99_us, row.max_us
        );
    }
    out
}

/// The reply in the Prometheus text format: counters as
/// `pdmount_<name>_total`, gauges as `pdmount_<name>`, latencies as the
/// summary `pdmount_latency_seconds`. Every series carries the profile, so
/// that several profiles can share one textfile directory.
fn prometheus(reply: &StatsReply, profile: &str) -> String {
    let profile = escape_label(profile);
    let mut out = String::new();
    for (name, value) in &reply.counters {
        let _ = writeln!(out, "# TYPE pdmount_{name}_total counter");
        let _ = writeln!(out, "pdmount_{name}_total{{profile=\"{profile}\"}} {value}");
    }
    for (name, value) in &reply.gauges {
        let _ = writeln!(out, "# TYPE pdmount_{name} gauge");
        let _ = writeln!(out, "pdmount_{name}{{profile=\"{profile}\"}} {value}");
    }
    if !reply.latency.is_empty() {
        let _ = writeln!(out, "# TYPE pdmount_latency_seconds summary");
    }
    for row in &reply.latency {
        let labels = format!("profile=\"{profile}\",op=\"{}\"", row.name);
        for (quantile, us) in [
            ("0.5", row.p50_us),
            ("0.9", row.p90_us),
            ("0.99", row.p99_us),
        ] {
            let _ = writeln!(
                out,
                "pdmount_latency_seconds{{{labels},quantile=\"{quantile}\"}} {}",
                us as f64 / 1e6
            );
        }
        let _ = writeln!(
            out,
            "pdmount_latency_seconds_count{{{labels}}} {}",
            row.count
        );
    }
    out
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Writes the textfile every 15 s until aborted.
pub fn spawn_textfile(
    engine: Arc<Engine>,
    path: PathBuf,
    profile: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TEXTFILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failing = false;
        loop {
            tick.tick().await;
            let text = prometheus(&reply(&engine, false), &profile);
            let target = path.clone();
            let result = tokio::task::spawn_blocking(move || write_atomically(&target, &text))
                .await
                .unwrap_or_else(|error| Err(std::io::Error::other(error)));
            match result {
                Ok(()) => failing = false,
                // Once per failure streak: the next tick would say the same.
                Err(error) if !failing => {
                    failing = true;
                    tracing::warn!(path = %path.display(), %error, "cannot write the Prometheus textfile");
                }
                Err(_) => {}
            }
        }
    })
}

/// Writes `text` next to `path` and renames it into place, so a collector
/// never reads half a file. The temporary name does not end in `.prom`.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("the textfile path has no file name"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample() -> StatsReply {
        StatsReply {
            counters: BTreeMap::from([("uploads".to_owned(), 3)]),
            pool: Vec::new(),
            gauges: BTreeMap::from([("queue_pending".to_owned(), 2)]),
            latency: vec![LatencyReply {
                name: "getattr".into(),
                count: 10,
                p50_us: 12,
                p90_us: 30,
                p99_us: 250,
                max_us: 1000,
            }],
        }
    }

    #[test]
    fn prometheus_text_has_counters_gauges_and_summaries() {
        let text = prometheus(&sample(), "de\"v");
        let expected = [
            "# TYPE pdmount_uploads_total counter",
            r#"pdmount_uploads_total{profile="de\"v"} 3"#,
            "# TYPE pdmount_queue_pending gauge",
            r#"pdmount_queue_pending{profile="de\"v"} 2"#,
            "# TYPE pdmount_latency_seconds summary",
            r#"pdmount_latency_seconds{profile="de\"v",op="getattr",quantile="0.5"} 0.000012"#,
            r#"pdmount_latency_seconds{profile="de\"v",op="getattr",quantile="0.9"} 0.00003"#,
            r#"pdmount_latency_seconds{profile="de\"v",op="getattr",quantile="0.99"} 0.00025"#,
            r#"pdmount_latency_seconds_count{profile="de\"v",op="getattr"} 10"#,
        ];
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn text_form_ends_with_the_latency_table() {
        let text = text(&sample());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "uploads        3");
        assert_eq!(lines[1], "queue_pending  2");
        assert!(lines[3].starts_with("latency (µs)"));
        assert_eq!(
            lines[4].split_whitespace().collect::<Vec<_>>(),
            ["getattr", "10", "12", "250", "1000"]
        );
    }

    #[test]
    fn the_textfile_is_replaced_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pdmount.prom");
        write_atomically(&path, "a 1\n").expect("first");
        write_atomically(&path, "a 2\n").expect("second");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "a 2\n");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["pdmount.prom"]);
    }
}
