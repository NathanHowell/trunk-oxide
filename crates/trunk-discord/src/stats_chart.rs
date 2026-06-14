//! Renders a PNG chart of key metrics pulled from `VictoriaMetrics` for posting
//! into the hourly summary thread.
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

const WIDTH: u32 = 1000;
const HEIGHT: u32 = 1000;
/// Trailing window charted, in seconds (one hour).
const WINDOW_SECS: i64 = 3600;
/// Resolution of the `query_range` step, in seconds.
const STEP_SECS: i64 = 60;
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
    queries: &'static [QuerySpec],
}

/// The four panels, top to bottom. Metric names keep their dots (VM does not
/// rewrite them on OTLP ingest) and counters have no `_total` suffix — both
/// verified against the live VM instance.
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        title: "Call Activity",
        y_desc: "calls",
        queries: &[
            QuerySpec {
                query: "trunk.discord.active_calls",
                label: Label::Fixed("active"),
            },
            QuerySpec {
                query: "trunk.discord.coalescing_calls",
                label: Label::Fixed("coalescing"),
            },
        ],
    },
    PanelSpec {
        title: "Pipeline Health",
        y_desc: "count / per-sec",
        queries: &[
            QuerySpec {
                query: "trunk.discord.live_handlers",
                label: Label::Fixed("live handlers"),
            },
            QuerySpec {
                query: "trunk.discord.unfinalized_handlers",
                label: Label::Fixed("unfinalized"),
            },
            QuerySpec {
                query: "rate(trunk.discord.attach_dropped[5m])",
                label: Label::Fixed("attach drop/s"),
            },
            QuerySpec {
                query: "rate(trunk.discord.events_dropped[5m])",
                label: Label::Fixed("event drop/s"),
            },
        ],
    },
    PanelSpec {
        title: "Transcription Latency",
        y_desc: "seconds",
        queries: &[
            QuerySpec {
                query: "histogram_quantile(0.5, sum(rate(trunk.discord.transcription_duration_seconds_bucket[5m])) by (le))",
                label: Label::Fixed("p50"),
            },
            QuerySpec {
                query: "histogram_quantile(0.9, sum(rate(trunk.discord.transcription_duration_seconds_bucket[5m])) by (le))",
                label: Label::Fixed("p90"),
            },
        ],
    },
    PanelSpec {
        title: "SDR Signal / Noise / Squelch",
        y_desc: "dB",
        queries: &[
            QuerySpec {
                query: "trunk.oxide.conventional.signal_power_db",
                label: Label::Tagged {
                    prefix: "sig tg",
                    tag: "talkgroup",
                },
            },
            QuerySpec {
                query: "trunk.oxide.conventional.noise_floor_db",
                label: Label::Tagged {
                    prefix: "noise tg",
                    tag: "talkgroup",
                },
            },
            QuerySpec {
                query: "trunk.oxide.conventional.adaptive_squelch_db",
                label: Label::Tagged {
                    prefix: "sqlch tg",
                    tag: "talkgroup",
                },
            },
        ],
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
                            Label::Tagged { prefix, tag } => format!(
                                "{prefix} {}",
                                r.metric.get(*tag).map_or("?", String::as_str)
                            ),
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
        render(&panels, start, end)
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(panels: &[Panel], start: i64, end: i64) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&WHITE)
            .map_err(|e| anyhow::anyhow!("chart fill: {e}"))?;
        let areas = root.split_evenly((panels.len().max(1), 1));
        for (area, panel) in areas.iter().zip(panels) {
            draw_panel(area, panel, start, end)?;
        }
        root.present()
            .map_err(|e| anyhow::anyhow!("chart present: {e}"))?;
    }

    let img =
        image::RgbImage::from_raw(WIDTH, HEIGHT, buf).context("chart buffer size mismatch")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding chart PNG")?;
    Ok(png)
}

// Unix-second timestamps fit f64 exactly for any realistic epoch; the x-axis
// formatter's cast back to i64 is likewise exact.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn draw_panel<DB>(area: &DrawingArea<DB, Shift>, panel: &Panel, start: i64, end: i64) -> Result<()>
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
    let (ymin, ymax) = (ymin - pad, ymax + pad);

    let mut chart = ChartBuilder::on(area)
        .caption(panel.title, ("sans-serif", 22))
        .margin(8)
        .x_label_area_size(28)
        .y_label_area_size(56)
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

    for (idx, s) in panel.series.iter().enumerate() {
        let color = Palette99::pick(idx).to_rgba();
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
                series: vec![Series {
                    label: "active".to_string(),
                    points: vec![(0, 1.0), (60, 3.0), (120, 2.0), (180, 5.0)],
                }],
            },
            Panel {
                title: "SDR Signal / Noise / Squelch",
                y_desc: "dB",
                series: vec![Series {
                    label: "sig tg 1".to_string(),
                    points: vec![(0, -40.0), (60, -38.5), (120, -42.0)],
                }],
            },
        ];

        let png = render(&panels, 0, 180).expect("render");
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
}
