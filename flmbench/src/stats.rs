/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::time::Duration;

use serde_derive::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latency {
    pub min_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile of an ascending, non-empty slice.
pub fn percentile(sorted: &[Duration], percent: f64) -> Duration {
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Summarize task latencies; `None` when there are none.
pub fn latency(mut samples: Vec<Duration>) -> Option<Latency> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let total: Duration = samples.iter().sum();
    Some(Latency {
        min_ms: ms(samples[0]),
        mean_ms: ms(total) / samples.len() as f64,
        p50_ms: ms(percentile(&samples, 50.0)),
        p90_ms: ms(percentile(&samples, 90.0)),
        p99_ms: ms(percentile(&samples, 99.0)),
        max_ms: ms(samples[samples.len() - 1]),
    })
}

/// Median of the values; `None` when there are none.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|v| Duration::from_millis(*v)).collect()
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted = millis(&[10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
        assert_eq!(percentile(&sorted, 50.0), Duration::from_millis(50));
        assert_eq!(percentile(&sorted, 90.0), Duration::from_millis(90));
        assert_eq!(percentile(&sorted, 99.0), Duration::from_millis(100));
        assert_eq!(percentile(&sorted, 0.0), Duration::from_millis(10));
    }

    #[test]
    fn latency_summarizes_unsorted_samples() {
        let stats = latency(millis(&[30, 10, 20])).unwrap();
        assert_eq!(stats.min_ms, 10.0);
        assert_eq!(stats.max_ms, 30.0);
        assert_eq!(stats.p50_ms, 20.0);
        assert!((stats.mean_ms - 20.0).abs() < 1e-9);
    }

    #[test]
    fn empty_input_has_no_statistics() {
        assert_eq!(latency(Vec::new()), None);
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_handles_odd_and_even_counts() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }
}
