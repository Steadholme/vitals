//! Pure-Rust analytics: rolling baseline, z-score anomaly detection, and a linear forecast.
//!
//! No external ML service and no C dependencies — every function here is a small, deterministic
//! numeric routine over a slice of `f64`s, unit-tested below. A metric's series is always passed
//! oldest-first (ascending `ts`), so the LAST element is the most recent sample.
//!
//! Detection is SELF-baselining: the latest value is scored against the mean + standard deviation
//! of the PRECEDING points in the window, so a host is flagged only when it drifts from its OWN
//! recent behaviour — never against a global/cross-host threshold.

use serde::Serialize;

/// Minimum number of preceding points required before the latest sample can be scored. Below this
/// the baseline is too thin to trust, so detection abstains (no anomaly).
pub const MIN_BASELINE: usize = 8;

/// Mean + (population) standard deviation of a window, plus the sample count it was built from.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Baseline {
    pub mean: f64,
    pub stddev: f64,
    pub n: usize,
}

impl Baseline {
    /// An empty baseline (no samples).
    pub fn empty() -> Self {
        Baseline {
            mean: 0.0,
            stddev: 0.0,
            n: 0,
        }
    }
}

/// Population mean + standard deviation of `values`.
pub fn baseline(values: &[f64]) -> Baseline {
    let n = values.len();
    if n == 0 {
        return Baseline::empty();
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    Baseline {
        mean,
        stddev: var.sqrt(),
        n,
    }
}

/// Signed z-score of `value` against `base`. A degenerate (zero-spread) baseline yields `0.0` —
/// a flat series never produces an anomaly by construction.
pub fn zscore(value: f64, base: &Baseline) -> f64 {
    if base.stddev <= f64::EPSILON {
        0.0
    } else {
        (value - base.mean) / base.stddev
    }
}

/// Detect whether the LATEST value in `values` (ascending `ts`) is an anomaly relative to the
/// baseline of the preceding points. Returns the signed z-score when `|z| >= z_threshold`, else
/// `None`. Abstains when there are too few preceding points or the baseline has no spread.
pub fn detect_latest(values: &[f64], z_threshold: f64) -> Option<f64> {
    if values.len() <= MIN_BASELINE {
        return None;
    }
    let (history, last) = values.split_at(values.len() - 1);
    let base = baseline(history);
    if base.stddev <= f64::EPSILON {
        return None;
    }
    let z = zscore(last[0], &base);
    if z.abs() >= z_threshold {
        Some(z)
    } else {
        None
    }
}

/// A short-term forecast: `points` projected ahead, the fitted `slope` per step, and a `band`
/// (the residual standard deviation of the fit) describing the uncertainty around each point.
#[derive(Debug, Clone, Serialize)]
pub struct Forecast {
    pub points: Vec<f64>,
    pub slope: f64,
    pub band: f64,
}

/// Project `steps` points beyond `values` with an ordinary least-squares line fit over the window
/// (index → value). A flat or single-point series forecasts a constant continuation with a zero
/// band. The `band` is the residual std-dev of the fit, so a noisy series widens its own band.
pub fn forecast(values: &[f64], steps: usize) -> Forecast {
    let n = values.len();
    if steps == 0 {
        return Forecast {
            points: Vec::new(),
            slope: 0.0,
            band: 0.0,
        };
    }
    if n < 2 {
        let last = values.last().copied().unwrap_or(0.0);
        return Forecast {
            points: vec![last; steps],
            slope: 0.0,
            band: 0.0,
        };
    }

    let nf = n as f64;
    let xbar = (nf - 1.0) / 2.0;
    let ybar = values.iter().sum::<f64>() / nf;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (i, &y) in values.iter().enumerate() {
        let dx = i as f64 - xbar;
        sxy += dx * (y - ybar);
        sxx += dx * dx;
    }
    let slope = if sxx <= f64::EPSILON { 0.0 } else { sxy / sxx };
    let intercept = ybar - slope * xbar;

    // Residual std-dev of the fit drives the forecast band.
    let mut sse = 0.0;
    for (i, &y) in values.iter().enumerate() {
        let yhat = intercept + slope * i as f64;
        let r = y - yhat;
        sse += r * r;
    }
    let band = (sse / nf).sqrt();

    let mut points = Vec::with_capacity(steps);
    for k in 1..=steps {
        let x = (n - 1 + k) as f64;
        points.push(intercept + slope * x);
    }
    Forecast {
        points,
        slope,
        band,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_mean_and_stddev() {
        let b = baseline(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!((b.mean - 5.0).abs() < 1e-9);
        assert!((b.stddev - 2.0).abs() < 1e-9);
        assert_eq!(b.n, 8);
    }

    #[test]
    fn flat_series_never_anomalous() {
        let values = vec![3.0; 20];
        assert_eq!(detect_latest(&values, 3.0), None);
    }

    #[test]
    fn spike_is_detected_against_own_history() {
        // A calm history then a sharp spike on the latest sample.
        let mut values = vec![10.0, 10.2, 9.8, 10.1, 9.9, 10.0, 10.3, 9.7, 10.0, 10.1];
        values.push(99.0);
        let z = detect_latest(&values, 3.0).expect("spike should be flagged");
        assert!(z > 3.0, "expected a large positive z-score, got {z}");
    }

    #[test]
    fn normal_jitter_is_not_flagged() {
        let values = vec![10.0, 10.2, 9.8, 10.1, 9.9, 10.0, 10.3, 9.7, 10.0, 10.1, 10.05];
        assert_eq!(detect_latest(&values, 3.0), None);
    }

    #[test]
    fn too_few_points_abstain() {
        let values = vec![1.0, 100.0];
        assert_eq!(detect_latest(&values, 3.0), None);
    }

    #[test]
    fn forecast_continues_a_line() {
        let values: Vec<f64> = (0..10).map(|i| i as f64).collect(); // 0,1,..,9
        let f = forecast(&values, 3);
        assert!((f.slope - 1.0).abs() < 1e-9);
        assert!((f.points[0] - 10.0).abs() < 1e-6);
        assert!((f.points[1] - 11.0).abs() < 1e-6);
        assert!((f.points[2] - 12.0).abs() < 1e-6);
        assert!(f.band < 1e-6, "a perfect line has no residual band");
    }

    #[test]
    fn forecast_flat_series_is_constant() {
        let values = vec![7.0; 12];
        let f = forecast(&values, 4);
        assert!(f.slope.abs() < 1e-9);
        assert!(f.points.iter().all(|p| (p - 7.0).abs() < 1e-9));
    }
}
