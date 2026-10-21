//! What the monitor says about itself.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

pub struct Metrics {
    updates: Counter<u64>,
    update_seconds: Histogram<f64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            updates: meter
                .u64_counter("trunk.monitor.updates")
                .with_description("Dashboard refreshes, by how they ended")
                .build(),
            update_seconds: meter
                .f64_histogram("trunk.monitor.update_seconds")
                .with_description("Seconds a dashboard refresh took, to its end")
                .with_boundaries(vec![1.0, 2.0, 5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0])
                .build(),
        }
    }

    /// A refresh ended as `result`: `ok`, `render`, `discord` or `timeout`.
    pub fn refreshed(&self, result: &'static str, took: Duration) {
        let attrs = [KeyValue::new("result", result)];
        self.updates.add(1, &attrs);
        self.update_seconds.record(took.as_secs_f64(), &attrs);
    }
}
