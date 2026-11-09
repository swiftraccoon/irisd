//! The arithmetic and text formatting `daemon-stats` and `soak-report` share:
//! rows of a statistics series, column extremes and means, frame rates and
//! stalls, counter growth, and bounded text.

use crate::logs::{self, Fields};

/// One statistics row: its timestamp in seconds since the epoch and its fields.
pub type Row = (f64, Fields);

/// The numeric values of `key` over the rows.
pub fn col(rows: &[Row], key: &str) -> Vec<f64> {
    rows.iter()
        .filter_map(|(_, fields)| logs::number(fields, key))
        .collect()
}

/// The largest numeric value of `key` over the rows.
pub fn col_max(rows: &[Row], key: &str) -> Option<f64> {
    maximum(&col(rows, key))
}

/// The smallest value, `None` of an empty list.
pub fn minimum(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::min)
}

/// The largest value, `None` of an empty list.
pub fn maximum(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

/// The arithmetic mean, `None` of an empty list.
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// The median (the mean of the two middle values for an even count), `None`
/// of an empty list.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    Some(if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    })
}

/// The sum of the positive steps: a cumulative counter's growth across resets.
pub fn growth(values: &[f64]) -> f64 {
    values
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).max(0.0))
        .sum()
}

/// Frames per second between consecutive rows of one stream; a counter reset
/// or a timestamp that does not advance yields no rate.
pub fn rates(rows: &[Row]) -> Vec<f64> {
    rows.windows(2)
        .filter_map(|pair| {
            let (t0, a) = &pair[0];
            let (t1, b) = &pair[1];
            let fa = logs::number(a, "frames")?;
            let fb = logs::number(b, "frames")?;
            (t1 > t0 && fb >= fa).then(|| (fb - fa) / (t1 - t0))
        })
        .collect()
}

/// The median of the positive rates (0.0 without one) and the count of stall
/// intervals: rates of zero or below half that median.
pub fn median_and_stalls(rates: &[f64]) -> (f64, usize) {
    let positive = rates
        .iter()
        .copied()
        .filter(|rate| *rate > 0.0)
        .collect::<Vec<_>>();
    let median = median(&positive).unwrap_or(0.0);
    let stalls = rates
        .iter()
        .filter(|&&rate| rate <= 0.0 || (median > 0.0 && rate < 0.5 * median))
        .count();
    (median, stalls)
}

/// The value to `digits` decimals, `-` when there is none.
pub fn decimals(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.digits$}"))
}

/// The first `max` characters of `text`.
pub fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: f64, frames: &str) -> Row {
        (t, Fields::from([("frames".to_owned(), frames.to_owned())]))
    }

    #[test]
    fn median_averages_the_two_middle_values() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
    }

    #[test]
    fn growth_ignores_counter_resets() {
        assert_eq!(growth(&[1.0, 3.0, 2.0, 5.0]), 5.0);
    }

    #[test]
    fn an_empty_list_has_no_mean_or_extremes() {
        assert_eq!(mean(&[]), None);
        assert_eq!(minimum(&[]), None);
        assert_eq!(maximum(&[]), None);
    }

    #[test]
    fn rates_skip_resets_and_stalled_clocks() {
        let rows = [
            row(0.0, "0"),
            row(10.0, "300"),
            row(10.0, "330"),
            row(20.0, "10"),
            row(30.0, "310"),
        ];
        assert_eq!(rates(&rows), [30.0, 30.0]);
    }

    #[test]
    fn a_stall_is_a_rate_of_zero_or_under_half_the_median() {
        assert_eq!(median_and_stalls(&[30.0, 30.0, 10.0, 0.0]), (30.0, 2));
        assert_eq!(median_and_stalls(&[]), (0.0, 0));
    }

    #[test]
    fn a_missing_value_renders_as_a_dash() {
        assert_eq!(decimals(None, 2), "-");
        assert_eq!(decimals(Some(1.0 / 3.0), 2), "0.33");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("é🦀x", 2), "é🦀");
        assert_eq!(truncate("ab", 5), "ab");
    }
}
