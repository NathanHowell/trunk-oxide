//! Renders a PNG chart of key metrics pulled from `VictoriaMetrics` for posting
//! into the #monitoring channel.
//!
//! `VictoriaMetrics` has no server-side image rendering (its `/render` is a
//! Graphite *data* subset), so we query its JSON API (`/api/v1/query_range`)
//! and draw the chart locally with `plotters`. The runtime image is distroless
//! (no fonts / fontconfig), so text rendering uses plotters' pure-Rust
//! `ab_glyph` backend with a font bundled into the binary — see
//! [`register_font`].

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, Result};
use plotters::coord::Shift;
use plotters::prelude::*;
use serde::Deserialize;

/// Bundled font, registered as "sans-serif" so plotters' default family
/// resolves on the fontless distroless runtime.
const FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// Grid columns; rows follow from the panel count.
const COLUMNS: usize = 2;
const WIDTH: u32 = 1600;
/// Pixel height of one grid row.
const ROW_HEIGHT: u32 = 340;
/// Trailing window charted, in seconds (six hours).
const WINDOW_SECS: i64 = 6 * 3600;
/// Resolution of the `query_range` step, in seconds.
const STEP_SECS: i64 = 120;
/// Per-request timeout for VM queries.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

const DEFAULT_BASE_URL: &str = "http://victoriametrics:8428";

/// Register the bundled font as the default "sans-serif" family. Idempotent in
/// practice; call once at startup before any chart is rendered. Without this,
/// plotters' text drawing fails on the fontless runtime.
pub fn register_font() {
    if plotters::style::register_font("sans-serif", FontStyle::Normal, FONT_BYTES).is_err() {
        tracing::warn!("failed to register bundled chart font; chart text may not render");
    }
}

/// How to derive a series' legend label from a query result.
enum Label {
    /// A single fixed label (query is expected to return one series).
    Fixed(&'static str),
    /// `"{prefix} {value-of-tag}"` (query returns one series per label value).
    Tagged {
        prefix: &'static str,
        tag: &'static str,
    },
}

struct QuerySpec {
    query: &'static str,
    label: Label,
}

struct PanelSpec {
    title: &'static str,
    y_desc: &'static str,
    /// Anchor the y-axis at zero (rates, counts, levels); off for dB panels.
    zero_floor: bool,
    queries: &'static [QuerySpec],
}

/// Dashboard panels, row-major in a [`COLUMNS`]-wide grid. Metric names keep
/// their dots (VM does not rewrite them on OTLP ingest) and counters have no
/// `_total` suffix — both verified against the live VM instance. Latency
/// panels use sum/count means: several of the underlying histograms have
/// coarse default buckets, which make quantiles collapse into one bucket.
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        title: "Calls started / min",
        y_desc: "calls/min",
        zero_floor: true,
        queries: &[QuerySpec {
            query: "sum by (short_name)(rate(trunk.oxide.calls_started{short_name!=\"\"}[5m]))*60",
            label: Label::Tagged {
                prefix: "",
                tag: "short_name",
            },
        }],
    },
    PanelSpec {
        title: "Recorder load",
        y_desc: "calls / denials per min",
        zero_floor: true,
        queries: &[
            QuerySpec {
                query: "trunk.oxide.active_calls",
                label: Label::Fixed("active calls"),
            },
            QuerySpec {
                query: "sum(rate(trunk.oxide.recorder_denied{short_name!=\"\"}[5m]))*60",
                label: Label::Fixed("denied/min (no recorder)"),
            },
        ],
    },
    PanelSpec {
        title: "RF power: signal vs noise floor (median)",
        y_desc: "dB",
        zero_floor: false,
        queries: &[
            QuerySpec {
                query: "histogram_quantile(0.5, sum by (le,short_name)(rate(trunk.oxide.signal_power_db_bucket[30m])))",
                label: Label::Tagged {
                    prefix: "sig",
                    tag: "short_name",
                },
            },
            QuerySpec {
                query: "histogram_quantile(0.5, sum by (le,short_name)(rate(trunk.oxide.noise_floor_db_bucket[30m])))",
                label: Label::Tagged {
                    prefix: "noise",
                    tag: "short_name",
                },
            },
        ],
    },
    PanelSpec {
        title: "Voice frame errors",
        y_desc: "errors/s",
        zero_floor: true,
        queries: &[QuerySpec {
            query: "sum by (short_name)(rate(trunk.oxide.voice_frame_errors[10m]))",
            label: Label::Tagged {
                prefix: "",
                tag: "short_name",
            },
        }],
    },
    PanelSpec {
        title: "Channelizer DSP time per block (mean)",
        y_desc: "µs",
        zero_floor: true,
        queries: &[QuerySpec {
            query: "sum by (source)(rate(trunk.oxide.channelizer.block_time_us_sum{stat=\"mean\"}[5m]))/sum by (source)(rate(trunk.oxide.channelizer.block_time_us_count{stat=\"mean\"}[5m]))",
            label: Label::Tagged {
                prefix: "source",
                tag: "source",
            },
        }],
    },
    PanelSpec {
        title: "trunk-oxide CPU (cores)",
        y_desc: "cores",
        zero_floor: true,
        queries: &[
            QuerySpec {
                query: "sum(rate(process.cpu.time[5m]))",
                label: Label::Fixed("process total"),
            },
            QuerySpec {
                query: "topk(5, sum by (thread)(rate(process.thread.cpu.time[5m])))",
                label: Label::Tagged {
                    prefix: "",
                    tag: "thread",
                },
            },
        ],
    },
    PanelSpec {
        title: "Active channels per source",
        y_desc: "channels",
        zero_floor: true,
        queries: &[QuerySpec {
            query: "trunk.oxide.channelizer.active_channels",
            label: Label::Tagged {
                prefix: "source",
                tag: "source",
            },
        }],
    },
    PanelSpec {
        title: "Latency (mean)",
        y_desc: "seconds",
        zero_floor: true,
        queries: &[
            QuerySpec {
                query: "sum(rate(trunk.parakeet.transcription_duration_seconds_sum[10m]))/sum(rate(trunk.parakeet.transcription_duration_seconds_count[10m]))",
                label: Label::Fixed("transcribe"),
            },
            QuerySpec {
                query: "sum(rate(trunk.parakeet.queue_wait_seconds_sum[10m]))/sum(rate(trunk.parakeet.queue_wait_seconds_count[10m]))",
                label: Label::Fixed("transcribe queue wait"),
            },
            QuerySpec {
                query: "sum by (operation)(rate(trunk.discord.discord_api_duration_seconds_sum[10m]))/sum by (operation)(rate(trunk.discord.discord_api_duration_seconds_count[10m]))",
                label: Label::Tagged {
                    prefix: "discord",
                    tag: "operation",
                },
            },
        ],
    },
    PanelSpec {
        title: "Backlog and drops",
        y_desc: "count",
        zero_floor: true,
        queries: &[
            QuerySpec {
                query: "trunk.parakeet.queue.depth",
                label: Label::Fixed("transcribe queue"),
            },
            QuerySpec {
                query: "trunk.discord.unfinalized_handlers",
                label: Label::Fixed("unfinalized handlers"),
            },
            QuerySpec {
                query: "sum(increase(trunk.discord.attach_dropped[10m]))",
                label: Label::Fixed("audio attach drops /10m"),
            },
            QuerySpec {
                query: "sum(increase(trunk.discord.events_dropped[10m]))",
                label: Label::Fixed("event drops /10m"),
            },
            QuerySpec {
                query: "sum(increase(trunk.oxide.channelizer.overruns[10m]))",
                label: Label::Fixed("channelizer overruns /10m"),
            },
        ],
    },
    PanelSpec {
        title: "Flagged calls",
        y_desc: "per 30 min",
        zero_floor: true,
        queries: &[QuerySpec {
            query: "sum by (reason)(increase(trunk.discord.calls_flagged[30m]))",
            label: Label::Tagged {
                prefix: "",
                tag: "reason",
            },
        }],
    },
];

/// One line on a panel.
struct Series {
    label: String,
    points: Vec<(i64, f64)>,
}

/// One rendered panel.
struct Panel {
    title: &'static str,
    y_desc: &'static str,
    zero_floor: bool,
    series: Vec<Series>,
}

// ── VM JSON response ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct VmResponse {
    data: VmData,
}

#[derive(Deserialize)]
struct VmData {
    result: Vec<VmResult>,
}

#[derive(Deserialize)]
struct VmResult {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    /// `[unix_seconds, "value"]` pairs.
    #[serde(default)]
    values: Vec<(f64, String)>,
}

/// Parse the values of a VM `query_range` result into `(timestamp, value)`
/// points, dropping any that fail to parse (e.g. `NaN`).
// Timestamps are whole seconds well within f64's exact-integer range; the
// cast back to i64 is exact for any realistic epoch.
#[allow(clippy::cast_possible_truncation)]
fn parse_points(values: &[(f64, String)]) -> Vec<(i64, f64)> {
    values
        .iter()
        // VM returns "NaN" for gaps (e.g. histogram_quantile with no samples);
        // these parse successfully as f64::NAN, so drop non-finite explicitly —
        // a NaN in a LineSeries corrupts the plot.
        .filter_map(|(t, v)| {
            let val = v.parse::<f64>().ok()?;
            val.is_finite().then_some((*t as i64, val))
        })
        .collect()
}

fn parse_response(body: &str) -> Result<Vec<VmResult>> {
    let resp: VmResponse = serde_json::from_str(body).context("parsing VM response")?;
    Ok(resp.data.result)
}

// ── Client ──────────────────────────────────────────────────────────────────

/// Queries `VictoriaMetrics` and renders the hourly stats chart.
pub struct StatsClient {
    http: reqwest::Client,
    base_url: String,
}

impl StatsClient {
    /// Build a client from the `VM_QUERY_URL` env var. Unset → default
    /// (`http://victoriametrics:8428`, reachable on the compose network);
    /// set-but-empty → `None` (feature disabled). Returns `None` if the HTTP
    /// client can't be built.
    pub fn from_env() -> Option<Self> {
        let base_url = match std::env::var("VM_QUERY_URL") {
            Ok(s) if s.trim().is_empty() => return None,
            Ok(s) => s,
            Err(_) => DEFAULT_BASE_URL.to_string(),
        };
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .ok()?;
        Some(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    async fn query_range(&self, query: &str, start: i64, end: i64) -> Result<Vec<VmResult>> {
        let body = self
            .http
            .get(format!("{}/api/v1/query_range", self.base_url))
            .query(&[
                ("query", query),
                ("start", &start.to_string()),
                ("end", &end.to_string()),
                ("step", &STEP_SECS.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_response(&body)
    }

    async fn fetch_panel(&self, spec: &PanelSpec, start: i64, end: i64) -> Panel {
        let mut series = Vec::new();
        for q in spec.queries {
            match self.query_range(q.query, start, end).await {
                Ok(results) => {
                    for r in results {
                        let label = match &q.label {
                            Label::Fixed(s) => (*s).to_string(),
                            Label::Tagged { prefix, tag } => {
                                let value = r.metric.get(*tag).map_or("?", String::as_str);
                                if prefix.is_empty() {
                                    value.to_string()
                                } else {
                                    format!("{prefix} {value}")
                                }
                            }
                        };
                        let points = parse_points(&r.values);
                        if !points.is_empty() {
                            series.push(Series { label, points });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(query = q.query, error = %e, "stats query failed");
                }
            }
        }
        Panel {
            title: spec.title,
            y_desc: spec.y_desc,
            zero_floor: spec.zero_floor,
            series,
        }
    }

    /// Fetch all panels and render the chart to PNG bytes.
    pub async fn render_chart(&self) -> Result<Vec<u8>> {
        let end = chrono::Utc::now().timestamp();
        let start = end - WINDOW_SECS;
        let mut panels = Vec::with_capacity(PANELS.len());
        for spec in PANELS {
            panels.push(self.fetch_panel(spec, start, end).await);
        }
        let restarts = match self.query_range(RESTART_QUERY, start, end).await {
            Ok(results) => restarts(&results),
            Err(e) => {
                tracing::warn!(error = %e, "restart query failed");
                Vec::new()
            }
        };
        render(&panels, &restarts, start, end)
    }
}

/// trunk-oxide's start time; where it changes, the recorder restarted.
const RESTART_QUERY: &str = "trunk.oxide.process.start_time";

/// When trunk-oxide (re)started within the window: each new start time the
/// gauge reports, at the first sample that reports it.
fn restarts(results: &[VmResult]) -> Vec<i64> {
    let mut out = Vec::new();
    for r in results {
        let mut last = None;
        for (t, v) in parse_points(&r.values) {
            #[allow(clippy::cast_possible_truncation)]
            let started = v as i64;
            if last.is_some_and(|l| l != started) {
                out.push(t);
            }
            last = Some(started);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(panels: &[Panel], restarts: &[i64], start: i64, end: i64) -> Result<Vec<u8>> {
    let rows = panels.len().div_ceil(COLUMNS).max(1);
    let height = ROW_HEIGHT * u32::try_from(rows).unwrap_or(1);
    let mut buf = vec![0u8; (WIDTH * height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (WIDTH, height)).into_drawing_area();
        root.fill(&WHITE)
            .map_err(|e| anyhow::anyhow!("chart fill: {e}"))?;
        let areas = root.split_evenly((rows, COLUMNS));
        for (area, panel) in areas.iter().zip(panels) {
            draw_panel(area, panel, restarts, start, end)?;
        }
        root.present()
            .map_err(|e| anyhow::anyhow!("chart present: {e}"))?;
    }

    let img =
        image::RgbImage::from_raw(WIDTH, height, buf).context("chart buffer size mismatch")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding chart PNG")?;
    Ok(png)
}

/// Stable per-system colours so a system reads the same on every panel;
/// anything else falls back to its position in the palette. Noise-floor lines
/// are drawn translucent so they pair visibly with their signal line.
fn series_color(label: &str, idx: usize) -> RGBAColor {
    let system = label.rsplit(' ').next().unwrap_or(label);
    let base = match system {
        "conv" => RGBColor(31, 119, 180),
        "smco" => RGBColor(44, 160, 44),
        "svrcs" => RGBColor(255, 127, 14),
        _ => return Palette99::pick(idx + 3).to_rgba(),
    };
    let alpha = if label.starts_with("noise ") {
        0.45
    } else {
        1.0
    };
    base.mix(alpha)
}

// Unix-second timestamps fit f64 exactly for any realistic epoch; the x-axis
// formatter's cast back to i64 is likewise exact.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn draw_panel<DB>(
    area: &DrawingArea<DB, Shift>,
    panel: &Panel,
    restarts: &[i64],
    start: i64,
    end: i64,
) -> Result<()>
where
    DB: DrawingBackend,
    DB::ErrorType: 'static,
{
    // y-bounds from the data, with a little padding; fall back to 0..1 when empty.
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    for s in &panel.series {
        for &(_, v) in &s.points {
            if v.is_finite() {
                ymin = ymin.min(v);
                ymax = ymax.max(v);
            }
        }
    }
    if !ymin.is_finite() {
        ymin = 0.0;
        ymax = 1.0;
    }
    if (ymax - ymin).abs() < f64::EPSILON {
        ymax = ymin + 1.0;
    }
    let pad = (ymax - ymin) * 0.1;
    let (ymin, ymax) = if panel.zero_floor {
        (0.0, ymax + pad)
    } else {
        (ymin - pad, ymax + pad)
    };

    let mut chart = ChartBuilder::on(area)
        .caption(panel.title, ("sans-serif", 20))
        .margin(8)
        .x_label_area_size(28)
        .y_label_area_size(64)
        .build_cartesian_2d(start as f64..end as f64, ymin..ymax)
        .map_err(|e| anyhow::anyhow!("chart build: {e}"))?;

    chart
        .configure_mesh()
        .y_desc(panel.y_desc)
        .x_labels(6)
        .x_label_formatter(&|x| {
            chrono::DateTime::from_timestamp(*x as i64, 0)
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default()
        })
        .draw()
        .map_err(|e| anyhow::anyhow!("chart mesh: {e}"))?;

    // A restart explains a step in anything: a thin grey line at each.
    for &t in restarts {
        chart
            .draw_series(LineSeries::new(
                [(t as f64, ymin), (t as f64, ymax)],
                BLACK.mix(0.35).stroke_width(1),
            ))
            .map_err(|e| anyhow::anyhow!("chart restart marker: {e}"))?;
    }

    for (idx, s) in panel.series.iter().enumerate() {
        let color = series_color(&s.label, idx);
        chart
            .draw_series(LineSeries::new(
                s.points.iter().map(|&(t, v)| (t as f64, v)),
                color.stroke_width(2),
            ))
            .map_err(|e| anyhow::anyhow!("chart series: {e}"))?
            .label(s.label.clone())
            .legend(move |(x, y)| {
                PathElement::new(vec![(x, y), (x + 18, y)], color.stroke_width(2))
            });
    }

    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperLeft)
        .label_font(("sans-serif", 14))
        .background_style(WHITE.mix(0.8))
        .border_style(BLACK.mix(0.3))
        .draw()
        .map_err(|e| anyhow::anyhow!("chart legend: {e}"))?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "status": "success",
        "data": {
            "resultType": "matrix",
            "result": [
                {
                    "metric": {"__name__": "trunk.oxide.conventional.signal_power_db", "talkgroup": "1"},
                    "values": [[1781110920, "0.024"], [1781110980, "-3.5"], [1781111040, "NaN"]]
                },
                {
                    "metric": {"__name__": "trunk.oxide.conventional.signal_power_db", "talkgroup": "2"},
                    "values": [[1781110920, "0"]]
                }
            ]
        }
    }"#;

    #[test]
    fn parses_vm_matrix_response() {
        let results = parse_response(SAMPLE).expect("parse");
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].metric.get("talkgroup").map(String::as_str),
            Some("1")
        );

        // NaN value is dropped; the two valid points survive.
        let points = parse_points(&results[0].values);
        assert_eq!(points, vec![(1781110920, 0.024), (1781110980, -3.5)]);
    }

    /// A restart is where the start-time gauge changes, not where it
    /// begins.
    #[test]
    fn restarts_are_changes_of_start_time() {
        let series = |values: &[(f64, &str)]| VmResult {
            metric: BTreeMap::new(),
            values: values.iter().map(|&(t, v)| (t, v.to_owned())).collect(),
        };
        let results = [
            series(&[
                (0.0, "100"),
                (60.0, "100"),
                (120.0, "130"),
                (180.0, "130"),
                (240.0, "200"),
            ]),
            series(&[(0.0, "100"), (120.0, "130")]),
        ];
        assert_eq!(restarts(&results), [120, 240]);
        assert!(restarts(&[series(&[(0.0, "100"), (60.0, "100")])]).is_empty());
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response("not json").is_err());
    }

    #[test]
    fn renders_png_with_bundled_font() {
        register_font();

        let panels = vec![
            Panel {
                title: "Call Activity",
                y_desc: "calls",
                zero_floor: true,
                series: vec![Series {
                    label: "active".to_string(),
                    points: vec![(0, 1.0), (60, 3.0), (120, 2.0), (180, 5.0)],
                }],
            },
            Panel {
                title: "SDR Signal / Noise / Squelch",
                y_desc: "dB",
                zero_floor: false,
                series: vec![Series {
                    label: "sig tg 1".to_string(),
                    points: vec![(0, -40.0), (60, -38.5), (120, -42.0)],
                }],
            },
        ];

        let png = render(&panels, &[60], 0, 180).expect("render");
        // PNG magic number — proves we produced a real image, font path included.
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            png.len() > 1000,
            "png suspiciously small: {} bytes",
            png.len()
        );

        // Best-effort artifact for eyeballing; ignore write failures.
        let _ = std::fs::write("/tmp/stats_chart_test.png", &png);
    }

    /// Render the dashboard against a live `VictoriaMetrics`:
    /// `VM_QUERY_URL=http://localhost:8428 STATS_PNG_OUT=/tmp/x.png cargo test -p trunk-discord live_render -- --ignored`
    #[tokio::test]
    #[ignore = "needs a live VictoriaMetrics"]
    async fn live_render() {
        register_font();
        let client = StatsClient::from_env().expect("VM_QUERY_URL");
        let png = client.render_chart().await.expect("render");
        let out = std::env::var("STATS_PNG_OUT").unwrap_or_else(|_| "/tmp/stats_live.png".into());
        std::fs::write(out, png).expect("write png");
    }
}
