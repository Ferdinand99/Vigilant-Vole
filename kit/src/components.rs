//! View-models for the design system's data components, the ones a macro
//! in `macros.html` draws from a list: the bar list ([`BarRow`],
//! `m::bar_list`), the line chart ([`LineChart`], `m::line_chart`) and code
//! with highlighted lines ([`CodeLine`], `m::code_lines`). See
//! `docs/design-system.md`.

/// One row of a bar list: a label (a link when `href` isn't empty), its
/// value, an optional second value and the bar's width.
#[derive(Debug, Clone, PartialEq)]
pub struct BarRow {
    pub label: String,
    /// Where the label links to; empty for plain text.
    pub href: String,
    /// The main value, formatted ("1,204").
    pub value: String,
    /// The third column ("42%"); empty in a two-column list.
    pub detail: String,
    /// The bar's width, 0–100 (percent of the list's width).
    pub share: f64,
}

impl BarRow {
    /// A row whose bar is `share` percent wide.
    pub fn new(label: impl Into<String>, value: impl Into<String>, share: f64) -> Self {
        BarRow {
            label: label.into(),
            href: String::new(),
            value: value.into(),
            detail: String::new(),
            share,
        }
    }

    /// Links the label.
    pub fn href(mut self, href: impl Into<String>) -> Self {
        self.href = href.into();
        self
    }

    /// The third column.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// `--share` for the row's style: "42.5%", clamped to 0–100.
    pub fn share_css(&self) -> String {
        let share = if self.share.is_finite() {
            self.share.clamp(0.0, 100.0)
        } else {
            0.0
        };
        format!("{}%", (share * 100.0).round() / 100.0)
    }
}

/// `part` as a percentage of `whole` (0 when `whole` is 0): a bar's share.
pub fn share(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        part / whole * 100.0
    } else {
        0.0
    }
}

/// Width and height of a chart's `viewBox` (the strokes don't scale).
pub const CHART_WIDTH: f64 = 600.0;
pub const CHART_HEIGHT: f64 = 120.0;

/// One line of a [`LineChart`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChartLine {
    /// The legend's name for it.
    pub label: String,
    /// A value shown next to the name ("1,204"); may be empty.
    pub value: String,
    /// `points` of its `<polyline>`.
    pub points: String,
    /// Its colour: `series-1` … `series-6` (the first is the brand's).
    pub series: usize,
}

/// Lines on one scale, from 0 to the largest value (`m::line_chart`). The
/// first line is drawn on top and may have an area under it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineChart {
    /// What the chart shows, for screen readers ("Visitors and pageviews,
    /// last 7 days").
    pub label: String,
    pub lines: Vec<ChartLine>,
    /// `points` of the first line's area; empty for none.
    pub area: String,
    /// The scale's labels, top and bottom ("1,200", "0").
    pub max_label: String,
    pub min_label: String,
    /// The time axis's labels, left and right ("7 days ago", "Today").
    pub start_label: String,
    pub end_label: String,
}

impl LineChart {
    /// A chart of `series` (`(name, values, shown value)`, values left to
    /// right, all of the same length), scaled to the largest value, which
    /// `max_label` names. With `area`, the first series gets a filled area.
    pub fn new(
        label: impl Into<String>,
        series: Vec<(String, Vec<f64>, String)>,
        area: bool,
    ) -> Self {
        let max = series
            .iter()
            .flat_map(|(_, values, _)| values.iter().copied())
            .filter(|v| v.is_finite())
            .fold(0.0_f64, f64::max);
        let lines: Vec<ChartLine> = series
            .into_iter()
            .enumerate()
            .map(|(i, (label, values, value))| ChartLine {
                label,
                value,
                points: chart_points(&values, max),
                series: i % 6 + 1,
            })
            .collect();
        let area = match lines.first() {
            Some(first) if area && !first.points.is_empty() => area_points(&first.points),
            _ => String::new(),
        };
        LineChart {
            label: label.into(),
            lines,
            area,
            ..LineChart::default()
        }
    }

    /// Sets the scale's labels.
    pub fn scale(mut self, max_label: impl Into<String>, min_label: impl Into<String>) -> Self {
        self.max_label = max_label.into();
        self.min_label = min_label.into();
        self
    }

    /// Sets the time axis's labels.
    pub fn axis(mut self, start_label: impl Into<String>, end_label: impl Into<String>) -> Self {
        self.start_label = start_label.into();
        self.end_label = end_label.into();
        self
    }
}

fn coord(v: f64) -> String {
    let rounded = (v * 10.0).round() / 10.0;
    format!("{rounded}")
}

/// `points` for a `<polyline>` in a [`CHART_WIDTH`] × [`CHART_HEIGHT`]
/// box: `values` spread evenly from left to right, 0 at the bottom and
/// `max` at the top (values outside are clamped). One value draws a flat
/// line; none draws nothing.
pub fn chart_points(values: &[f64], max: f64) -> String {
    let y = |v: f64| {
        let v = if v.is_finite() { v } else { 0.0 };
        let ratio = if max > 0.0 {
            (v / max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        CHART_HEIGHT - ratio * CHART_HEIGHT
    };
    match values {
        [] => String::new(),
        [only] => format!("0,{y} {CHART_WIDTH},{y}", y = coord(y(*only))),
        _ => {
            let step = CHART_WIDTH / (values.len() - 1) as f64;
            values
                .iter()
                .enumerate()
                .map(|(i, v)| format!("{},{}", coord(i as f64 * step), coord(y(*v))))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// The area under a line's `points`, closed along the bottom.
pub fn area_points(points: &str) -> String {
    format!("{points} {CHART_WIDTH},{CHART_HEIGHT} 0,{CHART_HEIGHT}")
}

/// One line of code in `m::code_lines`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeLine {
    /// Its number; empty for none.
    pub no: String,
    pub text: String,
    /// Highlighted (the line an error points at).
    pub current: bool,
}

impl CodeLine {
    /// Lines of `code`, numbered from `first`, with line `current`
    /// highlighted (if it is one of them).
    pub fn numbered(code: &str, first: usize, current: Option<usize>) -> Vec<CodeLine> {
        code.lines()
            .enumerate()
            .map(|(i, text)| CodeLine {
                no: (first + i).to_string(),
                text: text.to_string(),
                current: current == Some(first + i),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_carry_their_share_as_a_percentage() {
        let row = BarRow::new("/pricing", "1,204", share(1204.0, 2408.0))
            .href("/pages/pricing")
            .detail("50%");
        assert_eq!(row.share_css(), "50%");
        assert_eq!(BarRow::new("x", "1", 33.333_33).share_css(), "33.33%");
        assert_eq!(BarRow::new("x", "1", 140.0).share_css(), "100%");
        assert_eq!(BarRow::new("x", "1", f64::NAN).share_css(), "0%");
        assert_eq!(share(3.0, 0.0), 0.0);
        assert_eq!(row.href, "/pages/pricing");
    }

    #[test]
    fn lines_share_one_scale() {
        let chart = LineChart::new(
            "Visitors and pageviews",
            vec![
                ("Visitors".into(), vec![0.0, 50.0, 100.0], "100".into()),
                ("Pageviews".into(), vec![200.0, 100.0, 0.0], "200".into()),
            ],
            true,
        )
        .scale("200", "0")
        .axis("2 days ago", "Today");
        assert_eq!(chart.lines[0].points, "0,120 300,90 600,60");
        assert_eq!(chart.lines[1].points, "0,0 300,60 600,120");
        assert_eq!(chart.lines[1].series, 2);
        assert_eq!(chart.area, "0,120 300,90 600,60 600,120 0,120");
        assert_eq!(
            (chart.max_label.as_str(), chart.end_label.as_str()),
            ("200", "Today")
        );
    }

    #[test]
    fn short_and_empty_series_still_draw() {
        assert_eq!(chart_points(&[], 10.0), "");
        assert_eq!(chart_points(&[5.0], 10.0), "0,60 600,60");
        assert_eq!(chart_points(&[1.0, 2.0], 0.0), "0,120 600,120");
        assert_eq!(chart_points(&[f64::NAN, 30.0], 10.0), "0,120 600,0");
        let none = LineChart::new("x", Vec::new(), true);
        assert!(none.lines.is_empty() && none.area.is_empty());
    }

    #[test]
    fn code_lines_are_numbered_and_one_is_current() {
        let lines = CodeLine::numbered("fn main() {\n    run();\n}", 41, Some(42));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].no, "41");
        assert!(lines[1].current && !lines[0].current && !lines[2].current);
        assert_eq!(lines[1].text, "    run();");
    }
}
