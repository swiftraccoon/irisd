//! Time-range query parameters: `from`/`to` parsing, defaults, and bounds.

mod range;
mod time;

pub(crate) use range::{MAX_RANGE_US, TimeParams, resolve_range};
pub(crate) use time::parse_time;

#[cfg(test)]
mod tests {
    use iris_core::time::WallUs;

    use super::*;
    use crate::error::ApiError;
    use crate::query::range::DEFAULT_WINDOW_US;

    #[test]
    fn applies_defaults_and_bounds() {
        let now = WallUs::new(1_793_466_000_000_000);
        let range = resolve_range(&TimeParams::default(), now).unwrap();
        assert_eq!(range.to, now);
        assert_eq!(range.from, now.offset(-DEFAULT_WINDOW_US));
        let too_long = TimeParams {
            from: Some("0".into()),
            to: Some(now.get().to_string()),
        };
        assert!(matches!(
            resolve_range(&too_long, now),
            Err(ApiError::BadRequest(_))
        ));
        let inverted = TimeParams {
            from: Some(now.get().to_string()),
            to: Some("5".into()),
        };
        assert!(matches!(
            resolve_range(&inverted, now),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_ranges_whose_span_or_default_start_overflows() {
        for params in [
            TimeParams {
                from: Some(i64::MIN.to_string()),
                to: Some(i64::MAX.to_string()),
            },
            TimeParams {
                from: None,
                to: Some(i64::MIN.to_string()),
            },
        ] {
            assert!(matches!(
                resolve_range(&params, WallUs::new(0)),
                Err(ApiError::BadRequest(_))
            ));
        }
        let params = TimeParams {
            from: Some((i64::MIN + 1).to_string()),
            to: Some((i64::MIN + 1 + MAX_RANGE_US).to_string()),
        };
        assert!(resolve_range(&params, WallUs::new(0)).is_ok());
    }
}
