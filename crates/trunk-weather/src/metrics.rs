//! The weather, as metrics.

use opentelemetry::metrics::{Counter, Gauge, Meter};

use crate::obs::Obs;

pub struct Metrics {
    temperature: Gauge<f64>,
    humidity: Gauge<f64>,
    pressure: Gauge<f64>,
    wind_avg: Gauge<f64>,
    wind_gust: Gauge<f64>,
    wind_direction: Gauge<f64>,
    solar: Gauge<f64>,
    uv: Gauge<f64>,
    illuminance: Gauge<f64>,
    battery: Gauge<f64>,
    raining: Gauge<u64>,
    lightning_distance: Gauge<f64>,
    rain: Counter<f64>,
    lightning: Counter<u64>,
    rain_starts: Counter<u64>,
    connections: Counter<u64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        let gauge = |name: &'static str, unit: &'static str, description: &'static str| {
            meter
                .f64_gauge(name)
                .with_unit(unit)
                .with_description(description)
                .build()
        };
        Self {
            temperature: gauge("trunk.weather.temperature_c", "Cel", "Air temperature"),
            humidity: gauge("trunk.weather.humidity_pct", "%", "Relative humidity"),
            pressure: gauge("trunk.weather.pressure_hpa", "hPa", "Station pressure"),
            wind_avg: gauge(
                "trunk.weather.wind_avg_ms",
                "m/s",
                "Mean wind over the minute",
            ),
            wind_gust: gauge(
                "trunk.weather.wind_gust_ms",
                "m/s",
                "Strongest 3 s gust in the minute",
            ),
            wind_direction: gauge(
                "trunk.weather.wind_direction_deg",
                "deg",
                "Mean wind direction",
            ),
            solar: gauge("trunk.weather.solar_w_m2", "W/m2", "Solar radiation"),
            uv: gauge("trunk.weather.uv_index", "1", "UV index"),
            illuminance: gauge("trunk.weather.illuminance_lux", "lx", "Illuminance"),
            battery: gauge("trunk.weather.battery_v", "V", "Tempest battery voltage"),
            lightning_distance: gauge(
                "trunk.weather.lightning_distance_km",
                "km",
                "Mean distance of the minute's lightning strikes",
            ),
            raining: meter
                .u64_gauge("trunk.weather.raining")
                .with_description("1 while the last minute had rain or hail, else 0")
                .build(),
            rain: meter
                .f64_counter("trunk.weather.rain_mm")
                .with_unit("mm")
                .with_description("Rain fallen since the process started")
                .build(),
            lightning: meter
                .u64_counter("trunk.weather.lightning_strikes")
                .with_description("Lightning strikes detected")
                .build(),
            rain_starts: meter
                .u64_counter("trunk.weather.rain_starts")
                .with_description("Times the Tempest reported rain starting")
                .build(),
            connections: meter
                .u64_counter("trunk.weather.connections")
                .with_description("Attempts to connect to the Tempest cloud, by how they ended")
                .build(),
        }
    }

    pub fn observed(&self, obs: &Obs) {
        let set = |gauge: &Gauge<f64>, value: Option<f64>| {
            if let Some(v) = value {
                gauge.record(v, &[]);
            }
        };
        set(&self.temperature, obs.temperature_c);
        set(&self.humidity, obs.humidity_pct);
        set(&self.pressure, obs.pressure_hpa);
        set(&self.wind_avg, obs.wind_avg_ms);
        set(&self.wind_gust, obs.wind_gust_ms);
        set(&self.wind_direction, obs.wind_direction_deg);
        set(&self.solar, obs.solar_w_m2);
        set(&self.uv, obs.uv_index);
        set(&self.illuminance, obs.illuminance_lux);
        set(&self.battery, obs.battery_v);
        set(&self.lightning_distance, obs.lightning_distance_km);
        if let Some(mm) = obs.rain_mm {
            self.rain.add(mm.max(0.0), &[]);
        }
        if let Some(n) = obs.lightning_strikes {
            self.lightning.add(strikes(n), &[]);
        }
        if let Some(kind) = obs.precipitation_type {
            self.raining.record(u64::from(kind > 0.0), &[]);
        }
    }

    pub fn rain_started(&self) {
        self.rain_starts.add(1, &[]);
    }

    /// A connection attempt ended as `result`: `ok` (it streamed, then closed),
    /// `lookup` (no device to ask about), `connect` or `stream`.
    pub fn connection(&self, result: &'static str) {
        self.connections
            .add(1, &[opentelemetry::KeyValue::new("result", result)]);
    }
}

/// A strike count off the wire, which is a whole number sent as a float.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn strikes(n: f64) -> u64 {
    n.max(0.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_strike_count_is_a_whole_number_never_negative() {
        assert_eq!(strikes(3.0), 3);
        assert_eq!(strikes(0.0), 0);
        assert_eq!(strikes(-2.0), 0);
    }
}
