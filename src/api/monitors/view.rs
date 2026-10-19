//! Pure view helpers for the monitor pages: the server-rendered SVG
//! response-time chart, the 90-day uptime bar, and number/time formatting.

use std::fmt::Write as _;

use crate::db::heartbeats::ChartBucket;
use crate::db::rollups::DayStat;

pub const CHART_WIDTH: u32 = 600;
pub const CHART_HEIGHT: u32 = 140;
const CHART_PAD_TOP: f64 = 8.0;

/// Time range of the response-time chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    Hour,
    #[default]
    Day,
    Week,
    Month,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Hour, Range::Day, Range::Week, Range::Month];

    pub fn parse(s: &str) -> Self {
        match s {
            "1h" => Range::Hour,
            "7d" => Range::Week,
            "30d" => Range::Month,
            _ => Range::Day,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Range::Hour => "1h",
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }

    /// Segmented-control label.
    pub fn label(self) -> &'static str {
        match self {
            Range::Hour => crate::i18n::t("detail.range_1h"),
            Range::Day => crate::i18n::t("detail.range_24h"),
            Range::Week => crate::i18n::t("detail.range_7d"),
            Range::Month => crate::i18n::t("detail.range_30d"),
        }
    }

    /// Left end of the x axis.
    pub fn axis_start(self) -> &'static str {
        match self {
            Range::Hour => crate::i18n::t("detail.axis_1h"),
            Range::Day => crate::i18n::t("detail.axis_24h"),
            Range::Week => crate::i18n::t("detail.axis_7d"),
            Range::Month => crate::i18n::t("detail.axis_30d"),
        }
    }

    /// The window for an accessible sentence ("24 hours").
    pub fn aria_span(self) -> &'static str {
        match self {
            Range::Hour => crate::i18n::t("detail.aria_range_1h"),
            Range::Day => crate::i18n::t("detail.aria_range_24h"),
            Range::Week => crate::i18n::t("detail.aria_range_7d"),
            Range::Month => crate::i18n::t("detail.aria_range_30d"),
        }
    }

    /// Whole window in seconds.
    pub fn seconds(self) -> i64 {
        match self {
            Range::Hour => 3_600,
            Range::Day => 86_400,
            Range::Week => 7 * 86_400,
            Range::Month => 30 * 86_400,
        }
    }

    /// Number of buckets across the chart.
    pub fn buckets(self) -> i64 {
        match self {
            Range::Hour => 60,
            Range::Day => 144,
            Range::Week => 168,
            Range::Month => 30,
        }
    }

    pub fn bucket_seconds(self) -> i64 {
        self.seconds() / self.buckets()
    }

    /// Long ranges come from daily rollups (they survive retention).
    pub fn uses_rollups(self) -> bool {
        self == Range::Month
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub line: String,
    pub area: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Band {
    pub x: String,
    pub width: String,
    /// Maintenance (blue) instead of downtime (red).
    pub maintenance: bool,
}

/// Everything `partials/monitor_chart.html` draws.
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub range: Range,
    pub width: u32,
    pub height: u32,
    pub mid_y: u32,
    pub segments: Vec<Segment>,
    pub bands: Vec<Band>,
    pub has_down_band: bool,
    pub has_maintenance_band: bool,
    pub max_label: String,
    pub mid_label: String,
    pub has_data: bool,
    /// Average and peak of the plotted buckets.
    pub avg_label: String,
    pub peak_label: String,
    pub aria_label: String,
}

/// Rounds up to 1, 2, 2.5 or 5 × 10ⁿ so the scale reads cleanly.
pub fn nice_ceiling(v: f64) -> f64 {
    if v <= 1.0 {
        return 1.0;
    }
    let pow = 10f64.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if m * pow >= v {
            return m * pow;
        }
    }
    10.0 * pow
}

/// "120 ms", "1.5 s" ("1,5 s"), "12 s".
pub fn format_ms(ms: f64) -> String {
    if ms >= 10_000.0 {
        format!("{} s", crate::i18n::fmt_dec(ms / 1000.0, 0))
    } else if ms >= 1000.0 {
        let s = crate::i18n::fmt_dec(ms / 1000.0, 1);
        format!("{} s", s.trim_end_matches(".0").trim_end_matches(",0"))
    } else {
        format!("{} ms", ms.round() as i64)
    }
}

/// Builds the chart from sparse buckets (`index` in `0..range.buckets()`).
/// `expected_gap` is how many empty buckets in a row are still drawn as a
/// continuous line (≈ check interval / bucket size).
pub fn build_chart(range: Range, buckets: &[ChartBucket], expected_gap: i64) -> Chart {
    let n = range.buckets().max(2);
    let width = f64::from(CHART_WIDTH);
    let height = f64::from(CHART_HEIGHT);
    let plot_h = height - CHART_PAD_TOP;
    let step = width / (n - 1) as f64;
    let x_of = |i: i64| (i.clamp(0, n - 1) as f64) * step;

    let values: Vec<f64> = buckets.iter().filter_map(|b| b.avg_ms).filter(|v| v.is_finite()).collect();
    let peak = values.iter().copied().fold(0.0_f64, f64::max);
    let avg = if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 };
    let max = nice_ceiling(peak * 1.1);
    let y_of = |ms: f64| CHART_PAD_TOP + plot_h * (1.0 - (ms / max).clamp(0.0, 1.0));

    // Line segments, broken at long gaps and at buckets without a value.
    let tolerance = expected_gap.max(1) + 1;
    let mut segments = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut last_index: Option<i64> = None;
    let flush = |pts: &mut Vec<(f64, f64)>, out: &mut Vec<Segment>| {
        if pts.is_empty() {
            return;
        }
        if pts.len() == 1 {
            // A lone point: draw a short dash so it stays visible.
            let (x, y) = pts[0];
            *pts = vec![((x - 2.0).max(0.0), y), ((x + 2.0).min(width), y)];
        }
        let mut line = String::new();
        for (i, (x, y)) in pts.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            let _ = write!(line, "{x:.1},{y:.1}");
        }
        let (first_x, last_x) = (pts[0].0, pts[pts.len() - 1].0);
        let area = format!("{line} {last_x:.1},{height:.1} {first_x:.1},{height:.1}");
        out.push(Segment { line, area });
        pts.clear();
    };
    for b in buckets {
        if b.index < 0 || b.index >= n {
            continue;
        }
        let Some(ms) = b.avg_ms.filter(|v| v.is_finite()) else {
            flush(&mut current, &mut segments);
            last_index = None;
            continue;
        };
        if let Some(prev) = last_index
            && b.index - prev > tolerance
        {
            flush(&mut current, &mut segments);
        }
        current.push((x_of(b.index), y_of(ms)));
        last_index = Some(b.index);
    }
    flush(&mut current, &mut segments);

    // Downtime / maintenance bands, merged when adjacent.
    let mut bands: Vec<(i64, i64, bool)> = Vec::new();
    for b in buckets {
        let maintenance = if b.down > 0 {
            false
        } else if b.maintenance > 0 {
            true
        } else {
            continue;
        };
        match bands.last_mut() {
            Some((_, end, m)) if *m == maintenance && b.index - *end <= 1 => *end = b.index,
            _ => bands.push((b.index, b.index, maintenance)),
        }
    }
    let has_down_band = bands.iter().any(|(_, _, m)| !m);
    let has_maintenance_band = bands.iter().any(|(_, _, m)| *m);
    let bands = bands
        .into_iter()
        .map(|(start, end, maintenance)| {
            let x0 = (x_of(start) - step / 2.0).max(0.0);
            let x1 = (x_of(end) + step / 2.0).min(width);
            Band {
                x: format!("{x0:.1}"),
                width: format!("{:.1}", (x1 - x0).max(2.0)),
                maintenance,
            }
        })
        .collect();

    let has_data = !values.is_empty();
    Chart {
        range,
        width: CHART_WIDTH,
        height: CHART_HEIGHT,
        mid_y: (CHART_PAD_TOP + plot_h / 2.0).round() as u32,
        segments,
        bands,
        has_down_band,
        has_maintenance_band,
        max_label: format_ms(max),
        mid_label: format_ms(max / 2.0),
        has_data,
        avg_label: if has_data { format_ms(avg) } else { "—".into() },
        peak_label: if has_data { format_ms(peak) } else { "—".into() },
        aria_label: if has_data {
            crate::i18n::tf(
                "detail.chart_aria",
                &[("range", range.aria_span()), ("avg", &format_ms(avg)), ("peak", &format_ms(peak))],
            )
        } else {
            crate::i18n::t("detail.chart_aria_empty").into()
        },
    }
}

/// Daily rollups → chart buckets (for the long ranges).
pub fn buckets_from_days(days: &[DayStat]) -> Vec<ChartBucket> {
    days.iter()
        .enumerate()
        .filter(|(_, d)| d.total() > 0)
        .map(|(i, d)| ChartBucket {
            index: i as i64,
            avg_ms: d.avg_ms,
            down: d.down,
            maintenance: d.maintenance,
            total: d.total(),
        })
        .collect()
}

/// One cell of the 90-day uptime bar.
#[derive(Debug, Clone, PartialEq)]
pub struct UptimeDay {
    /// `status-up` | `status-pending` (partial) | `status-down` | `status-maintenance` | `status-none`.
    pub class: &'static str,
    pub tip: String,
}

pub fn uptime_days(days: &[DayStat]) -> Vec<UptimeDay> {
    days.iter()
        .map(|d| {
            let label = short_date(&d.day);
            let (class, what) = match d.uptime_pct {
                None if d.maintenance > 0 => ("status-maintenance", crate::i18n::t("status.maintenance").to_string()),
                None => ("status-none", crate::i18n::t("detail.no_data").to_string()),
                Some(p) if d.down == 0 => ("status-up", day_uptime(p)),
                Some(p) if p >= 90.0 => ("status-pending", day_uptime(p)),
                Some(p) => ("status-down", day_uptime(p)),
            };
            UptimeDay {
                class,
                tip: format!("{label} · {what}"),
            }
        })
        .collect()
}

fn day_uptime(p: f64) -> String {
    crate::i18n::tf("detail.day_uptime", &[("value", &format_pct(p))])
}

/// "99.95%", "100%" ("99,95 %" in Norwegian). Never rounds a partial
/// outage up to 100%.
pub fn format_pct(p: f64) -> String {
    crate::i18n::fmt_pct2(p)
}

/// "2026-10-03" (or a full timestamp) → "Oct 3" / "3. okt.". Unparseable
/// input is returned as is.
pub fn short_date(day: &str) -> String {
    day.get(..10)
        .and_then(|d| d.parse::<jiff::civil::Date>().ok())
        .map_or_else(|| day.to_string(), crate::i18n::fmt_day_month)
}

/// "2026-10-03 14:05:09" → "Oct 3, 14:05 UTC".
pub fn short_datetime(ts: &str) -> String {
    match ts.get(11..16) {
        Some(hm) => format!("{}, {hm} UTC", short_date(ts)),
        None => short_date(ts),
    }
}

/// Human duration: "45 sec", "12 min", "3 h 4 min", "2 d 5 h".
pub fn format_duration(secs: i64) -> String {
    crate::i18n::fmt_duration(secs)
}

/// "every 60 sec" style interval text: "30 sec", "5 min", "1 h 30 min".
pub fn format_interval(secs: i64) -> String {
    format_duration(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(index: i64, ms: Option<f64>, down: i64) -> ChartBucket {
        ChartBucket {
            index,
            avg_ms: ms,
            down,
            maintenance: 0,
            total: 1,
        }
    }

    #[test]
    fn nice_ceiling_rounds_to_readable_steps() {
        assert_eq!(nice_ceiling(0.0), 1.0);
        assert_eq!(nice_ceiling(87.0), 100.0);
        assert_eq!(nice_ceiling(120.0), 200.0);
        assert_eq!(nice_ceiling(201.0), 250.0);
        assert_eq!(nice_ceiling(3_300.0), 5_000.0);
        assert_eq!(nice_ceiling(9_000.0), 10_000.0);
    }

    #[test]
    fn ms_formatting() {
        assert_eq!(format_ms(120.4), "120 ms");
        assert_eq!(format_ms(1000.0), "1 s");
        assert_eq!(format_ms(1500.0), "1.5 s");
        assert_eq!(format_ms(25_000.0), "25 s");
    }

    #[test]
    fn chart_breaks_lines_at_gaps_and_marks_downtime() {
        let buckets = vec![
            bucket(0, Some(100.0), 0),
            bucket(1, Some(200.0), 0),
            bucket(2, None, 1),
            bucket(3, None, 1),
            bucket(4, Some(100.0), 0),
            bucket(40, Some(50.0), 0),
        ];
        let chart = build_chart(Range::Hour, &buckets, 1);
        assert!(chart.has_data);
        // [0,1], [4], [40] → three segments (the down buckets break the line, 4→40 is a gap).
        assert_eq!(chart.segments.len(), 3);
        assert_eq!(chart.bands.len(), 1, "adjacent down buckets merge");
        assert!(!chart.bands[0].maintenance);
        assert_eq!(chart.max_label, "250 ms");
        assert_eq!(chart.peak_label, "200 ms");
        // A lone point is drawn as a short dash (two points).
        assert_eq!(chart.segments[2].line.split(' ').count(), 2);
        // Points stay inside the viewBox.
        for seg in &chart.segments {
            for pt in seg.line.split(' ') {
                let (x, y) = pt.split_once(',').unwrap();
                let (x, y): (f64, f64) = (x.parse().unwrap(), y.parse().unwrap());
                assert!((0.0..=f64::from(CHART_WIDTH)).contains(&x));
                assert!((0.0..=f64::from(CHART_HEIGHT)).contains(&y));
            }
        }
    }

    #[test]
    fn empty_chart_has_no_data() {
        let chart = build_chart(Range::Day, &[], 1);
        assert!(!chart.has_data && chart.segments.is_empty() && chart.bands.is_empty());
        assert_eq!(chart.avg_label, "—");
    }

    #[test]
    fn chart_ignores_out_of_range_buckets() {
        let chart = build_chart(Range::Hour, &[bucket(-1, Some(5.0), 1), bucket(60, Some(5.0), 0)], 1);
        assert!(chart.segments.is_empty());
    }

    #[test]
    fn uptime_days_classify_partial_outages() {
        let day = |day: &str, up: i64, down: i64, maintenance: i64| DayStat {
            day: day.into(),
            up,
            down,
            pending: 0,
            maintenance,
            avg_ms: None,
            uptime_pct: (up + down > 0).then(|| 100.0 * up as f64 / (up + down) as f64),
        };
        let cells = uptime_days(&[
            day("2026-10-01", 0, 0, 0),
            day("2026-10-02", 100, 0, 0),
            day("2026-10-03", 99, 1, 0),
            day("2026-10-04", 1, 9, 0),
            day("2026-10-05", 0, 0, 5),
        ]);
        let classes: Vec<_> = cells.iter().map(|c| c.class).collect();
        assert_eq!(
            classes,
            vec!["status-none", "status-up", "status-pending", "status-down", "status-maintenance"]
        );
        assert_eq!(cells[2].tip, "Oct 3 · 99.00% uptime");
        assert_eq!(cells[0].tip, "Oct 1 · No data");
    }

    #[test]
    fn pct_never_rounds_up_to_100() {
        assert_eq!(format_pct(99.999), "99.99%");
        assert_eq!(format_pct(100.0), "100%");
        assert_eq!(format_pct(0.0), "0.00%");
    }

    #[test]
    fn dates_and_durations() {
        assert_eq!(short_date("2026-01-09"), "Jan 9");
        assert_eq!(short_date("2026-12-31 23:59:59"), "Dec 31");
        assert_eq!(short_date("garbage"), "garbage");
        assert_eq!(short_datetime("2026-10-03 14:05:09"), "Oct 3, 14:05 UTC");
        assert_eq!(format_duration(42), "42 sec");
        assert_eq!(format_duration(60), "1 min");
        assert_eq!(format_duration(3_600), "1 h");
        assert_eq!(format_duration(3_660), "1 h 1 min");
        assert_eq!(format_duration(2 * 86_400 + 5 * 3_600 + 7), "2 d 5 h");
        assert_eq!(format_duration(86_400 + 59), "1 d");
        assert_eq!(format_duration(-5), "0 sec");
    }
}
