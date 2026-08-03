//! The flat table renderer (MVP §3).
//!
//! Two rules earn this module its existence:
//!
//! - A cell is an enum, not a string. A report cannot accidentally hand the
//!   renderer a `0` for a value its adapter cannot populate — it must say
//!   [`Cell::Unsupported`], which renders as a dim `–` (MVP §3: "Reports grey
//!   out unsupported columns rather than printing a misleading `0`").
//! - Estimates are marked at the point of rendering: [`Cell::Money`] carries an
//!   `estimated` flag, prints a trailing `~`, and triggers the legend footer.

use std::fmt::Write as _;
use std::io::IsTerminal;

/// Placeholder for a KPI the source adapter cannot populate.
pub const UNSUPPORTED: &str = "–";

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GUTTER: usize = 2;
const LEGEND: &str = "~ estimated";

/// Whether rendering may emit ANSI escapes.
///
/// Escapes are only ever produced for a real terminal: never into `--json`,
/// never into a pipe or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// No escape codes at all. The right choice for tests and pipes.
    pub fn plain() -> Self {
        Self { color: false }
    }

    /// Colour only when stdout is a TTY.
    pub fn auto() -> Self {
        Self {
            color: std::io::stdout().is_terminal(),
        }
    }
}

/// One rendered value. Numeric variants align right; text aligns left.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Text(String),
    /// A count, rendered with human suffixes (`182.3k`, `3.42M`).
    Int(i64),
    /// A ratio or rate, rendered with `decimals` places.
    Float(f64, usize),
    /// A currency amount. `estimated` prints a `~` and adds the legend footer.
    Money {
        amount: f64,
        estimated: bool,
    },
    /// The adapter cannot populate this KPI. Visually distinct from `0`.
    Unsupported,
    /// Genuinely nothing to show here (e.g. a spacer in a totals row).
    Empty,
}

impl Cell {
    pub fn text(s: impl Into<String>) -> Self {
        Cell::Text(s.into())
    }

    /// A cost estimate — the only kind warden produces (MVP §2.5).
    pub fn money_est(amount: f64) -> Self {
        Cell::Money {
            amount,
            estimated: true,
        }
    }

    fn is_numeric(&self) -> bool {
        matches!(self, Cell::Int(_) | Cell::Float(..) | Cell::Money { .. })
    }

    fn is_estimate(&self) -> bool {
        matches!(
            self,
            Cell::Money {
                estimated: true,
                ..
            }
        )
    }

    /// The visible text, with no escape codes and therefore the true width.
    fn plain(&self) -> String {
        match self {
            Cell::Text(s) => s.clone(),
            Cell::Int(n) => format_count(*n),
            Cell::Float(f, decimals) => format!("{f:.*}", *decimals),
            Cell::Money { amount, estimated } => format_money(*amount, *estimated),
            Cell::Unsupported => UNSUPPORTED.to_string(),
            Cell::Empty => String::new(),
        }
    }
}

/// Human-readable counts: plain below 1000, `k` below a million, then `M`.
pub fn format_count(n: i64) -> String {
    let abs = (n as f64).abs();
    if abs < 1_000.0 {
        format!("{n}")
    } else if abs < 1_000_000.0 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    }
}

/// Currency, with a trailing `~` when the figure is an estimate.
pub fn format_money(amount: f64, estimated: bool) -> String {
    if estimated {
        format!("${amount:.2} ~")
    } else {
        format!("${amount:.2}")
    }
}

/// A headed, dynamically-sized table.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    /// Headers are uppercased for you; callers write them naturally.
    pub fn new<S: Into<String>>(headers: impl IntoIterator<Item = S>) -> Self {
        Self {
            headers: headers
                .into_iter()
                .map(|h| h.into().to_uppercase())
                .collect(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
    }

    pub fn with_row(mut self, row: Vec<Cell>) -> Self {
        self.push(row);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render with the given style. Lines carry no trailing whitespace.
    pub fn render(&self, style: Style) -> String {
        let cols = self.headers.len();
        let plain: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| (0..cols).map(|i| cell(row, i).plain()).collect())
            .collect();

        let mut widths: Vec<usize> = self.headers.iter().map(|h| width(h)).collect();
        for row in &plain {
            for (i, text) in row.iter().enumerate() {
                widths[i] = widths[i].max(width(text));
            }
        }

        // A column is numeric if any populated cell in it is numeric.
        let numeric: Vec<bool> = (0..cols)
            .map(|i| self.rows.iter().any(|row| cell(row, i).is_numeric()))
            .collect();

        let mut out = String::new();
        let mut line = String::new();
        for (i, header) in self.headers.iter().enumerate() {
            pad(&mut line, header, widths[i], numeric[i], i + 1 == cols);
        }
        push_line(&mut out, &line);

        for (r, row) in plain.iter().enumerate() {
            line.clear();
            for (i, text) in row.iter().enumerate() {
                let dim = style.color && matches!(cell(&self.rows[r], i), Cell::Unsupported);
                if dim {
                    line.push_str(DIM);
                }
                pad(&mut line, text, widths[i], numeric[i], i + 1 == cols);
                if dim {
                    line.push_str(RESET);
                }
            }
            push_line(&mut out, &line);
        }

        if self.rows.iter().flatten().any(Cell::is_estimate) {
            let total: usize = widths.iter().sum::<usize>() + GUTTER * cols.saturating_sub(1);
            let indent = total.saturating_sub(width(LEGEND));
            let _ = writeln!(out, "{:indent$}{LEGEND}", "");
        }
        out
    }
}

fn cell(row: &[Cell], i: usize) -> &Cell {
    row.get(i).unwrap_or(&Cell::Empty)
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn pad(line: &mut String, text: &str, w: usize, right: bool, last: bool) {
    let fill = w.saturating_sub(width(text));
    if right {
        for _ in 0..fill {
            line.push(' ');
        }
        line.push_str(text);
    } else {
        line.push_str(text);
        if !last {
            for _ in 0..fill {
                line.push(' ');
            }
        }
    }
    if !last {
        for _ in 0..GUTTER {
            line.push(' ');
        }
    }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line.trim_end());
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        Table::new(["project", "sessions", "in", "est. cost"])
            .with_row(vec![
                Cell::text("acme-api"),
                Cell::Int(41),
                Cell::Int(182_300),
                Cell::money_est(12.40),
            ])
            .with_row(vec![
                Cell::text("dotfiles"),
                Cell::Int(3),
                Cell::Unsupported,
                Cell::money_est(0.42),
            ])
    }

    #[test]
    fn counts_switch_units_at_the_k_and_m_boundaries() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.0k");
        assert_eq!(format_count(182_300), "182.3k");
        assert_eq!(format_count(999_999), "1000.0k");
        assert_eq!(format_count(1_000_000), "1.00M");
        assert_eq!(format_count(3_420_000), "3.42M");
        assert_eq!(format_count(-1_500), "-1.5k");
    }

    #[test]
    fn money_marks_estimates() {
        assert_eq!(format_money(12.4, true), "$12.40 ~");
        assert_eq!(format_money(12.4, false), "$12.40");
    }

    #[test]
    fn headers_are_uppercased_and_columns_line_up() {
        let rendered = sample().render(Style::plain());
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0], "PROJECT   SESSIONS      IN  EST. COST");
        assert_eq!(lines[1], "acme-api        41  182.3k   $12.40 ~");
        assert_eq!(lines[2], "dotfiles         3       –    $0.42 ~");

        // Right-aligned numerics: every row ends its money column at the same
        // column, and the header row is exactly as wide as the widest row.
        assert!(lines[1].ends_with("$12.40 ~"));
        assert!(lines[2].ends_with("$0.42 ~"));
        assert_eq!(lines[1].chars().count(), lines[2].chars().count());
        assert_eq!(lines[0].chars().count(), lines[1].chars().count());
    }

    #[test]
    fn unsupported_is_visually_distinct_from_zero() {
        let rendered = sample().render(Style::plain());
        assert!(rendered.contains(UNSUPPORTED));
        assert_ne!(UNSUPPORTED, "0");
        assert_eq!(Cell::Unsupported.plain(), "–");
        assert_eq!(Cell::Int(0).plain(), "0");
        assert_eq!(Cell::Empty.plain(), "");
    }

    #[test]
    fn no_ansi_escapes_when_not_a_tty() {
        let rendered = sample().render(Style::plain());
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
    }

    #[test]
    fn unsupported_cells_are_dimmed_when_colour_is_allowed() {
        let rendered = sample().render(Style { color: true });
        assert!(rendered.contains(DIM));
        assert!(rendered.contains(RESET));
        // Nothing else picks up escapes.
        assert_eq!(rendered.matches(DIM).count(), 1);
    }

    #[test]
    fn legend_appears_only_when_an_estimate_is_present() {
        assert!(sample().render(Style::plain()).ends_with("~ estimated\n"));

        let exact = Table::new(["tool", "calls"]).with_row(vec![Cell::text("Read"), Cell::Int(12)]);
        assert!(!exact.render(Style::plain()).contains(LEGEND));
    }

    #[test]
    fn short_rows_and_empty_tables_render_without_panicking() {
        let t = Table::new(["a", "b", "c"]).with_row(vec![Cell::text("x")]);
        assert_eq!(t.render(Style::plain()).lines().count(), 2);
        assert_eq!(
            Table::new(["a"]).render(Style::plain()),
            "A\n",
            "an empty table still prints its header"
        );
    }

    #[test]
    fn no_line_has_trailing_whitespace() {
        for line in sample().render(Style::plain()).lines() {
            assert_eq!(line, line.trim_end());
        }
    }
}
