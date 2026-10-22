//! What the Tempest cloud says over its WebSocket, and the station lookup that
//! finds the device to ask about.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One minute's report from a Tempest (`obs_st`); a sensor that had nothing
/// to say is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Obs {
    pub wind_lull_ms: Option<f64>,
    pub wind_avg_ms: Option<f64>,
    pub wind_gust_ms: Option<f64>,
    pub wind_direction_deg: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub illuminance_lux: Option<f64>,
    pub uv_index: Option<f64>,
    pub solar_w_m2: Option<f64>,
    /// Rain that fell in the minute.
    pub rain_mm: Option<f64>,
    /// 0 none, 1 rain, 2 hail, 3 rain and hail.
    pub precipitation_type: Option<f64>,
    pub lightning_distance_km: Option<f64>,
    /// Strikes in the minute.
    pub lightning_strikes: Option<f64>,
    pub battery_v: Option<f64>,
}

impl Obs {
    /// The fields of an `obs_st` row, in the order the API lists them (index
    /// 0 is the time and 5 the wind sample interval, neither of which we keep).
    fn from_row(row: &[Option<f64>]) -> Option<Self> {
        // the last field we read is the battery, at 16
        if row.len() < 17 {
            return None;
        }
        let at = |i: usize| row[i];
        Some(Self {
            wind_lull_ms: at(1),
            wind_avg_ms: at(2),
            wind_gust_ms: at(3),
            wind_direction_deg: at(4),
            pressure_hpa: at(6),
            temperature_c: at(7),
            humidity_pct: at(8),
            illuminance_lux: at(9),
            uv_index: at(10),
            solar_w_m2: at(11),
            rain_mm: at(12),
            precipitation_type: at(13),
            lightning_distance_km: at(14),
            lightning_strikes: at(15),
            battery_v: at(16),
        })
    }
}

/// A message from the stream, as far as we care.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Observation(Box<Obs>),
    /// Rain has started.
    RainStart,
    /// Anything else: acknowledgements, the connection notice, other devices.
    Other,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    obs: Vec<Vec<Option<f64>>>,
}

/// Read one text frame.
pub fn parse(text: &str) -> Result<Message> {
    let envelope: Envelope = serde_json::from_str(text).context("not a weather message")?;
    Ok(match envelope.kind.as_str() {
        "obs_st" => envelope
            .obs
            .first()
            .and_then(|row| Obs::from_row(row))
            .map_or(Message::Other, |obs| Message::Observation(Box::new(obs))),
        "evt_precip" => Message::RainStart,
        _ => Message::Other,
    })
}

#[derive(Deserialize)]
struct Stations {
    stations: Vec<Station>,
}

#[derive(Deserialize)]
struct Station {
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Deserialize)]
struct Device {
    device_id: u64,
    device_type: String,
}

/// The device id of the first Tempest (`ST`) in a stations reply; the hub
/// (`HB`) has no weather to report.
pub fn tempest_device(stations: &str) -> Result<u64> {
    let stations: Stations = serde_json::from_str(stations).context("not a stations reply")?;
    for device in stations.stations.iter().flat_map(|s| &s.devices) {
        if device.device_type == "ST" {
            return Ok(device.device_id);
        }
    }
    bail!("the account has no Tempest (device type ST)")
}

#[cfg(test)]
mod tests {
    use super::*;

    // from WeatherFlow's UDP/WebSocket documentation
    const OBS: &str = r#"{"type":"obs_st","device_id":1110,"obs":[[1588948614,0.18,0.22,0.27,144,6,1017.57,22.37,50.26,328,0.03,3,0.00,0,0,0,2.410,1]]}"#;

    #[test]
    fn an_observation_carries_the_weather_we_chart() {
        let Message::Observation(obs) = parse(OBS).unwrap() else {
            panic!("not an observation")
        };
        assert_eq!(obs.temperature_c, Some(22.37));
        assert_eq!(obs.humidity_pct, Some(50.26));
        assert_eq!(obs.pressure_hpa, Some(1017.57));
        assert_eq!(obs.wind_avg_ms, Some(0.22));
        assert_eq!(obs.wind_gust_ms, Some(0.27));
        assert_eq!(obs.rain_mm, Some(0.0));
        assert_eq!(obs.precipitation_type, Some(0.0));
        assert_eq!(obs.battery_v, Some(2.41));
    }

    #[test]
    fn a_sensor_with_nothing_to_say_is_none_not_an_error() {
        let text = OBS.replace("22.37", "null");
        let Message::Observation(obs) = parse(&text).unwrap() else {
            panic!("not an observation")
        };
        assert_eq!(obs.temperature_c, None);
        assert_eq!(obs.humidity_pct, Some(50.26));
    }

    #[test]
    fn a_short_row_is_ignored() {
        let text = r#"{"type":"obs_st","obs":[[1588948614,0.18]]}"#;
        assert_eq!(parse(text).unwrap(), Message::Other);
    }

    #[test]
    fn rain_starting_is_an_event() {
        let text = r#"{"type":"evt_precip","device_id":1110,"evt":[1493322445]}"#;
        assert_eq!(parse(text).unwrap(), Message::RainStart);
    }

    #[test]
    fn acknowledgements_and_other_messages_are_other() {
        for text in [
            r#"{"type":"ack","id":"2098388936"}"#,
            r#"{"type":"connection_opened"}"#,
            r#"{"type":"evt_strike","evt":[1493322445,27,3848]}"#,
        ] {
            assert_eq!(parse(text).unwrap(), Message::Other, "{text}");
        }
    }

    #[test]
    fn something_that_is_not_a_message_is_an_error() {
        assert!(parse("hello").is_err());
    }

    #[test]
    fn the_tempest_is_the_station_device_that_is_not_the_hub() {
        let reply = r#"{"stations":[{"station_id":9,"devices":[
            {"device_id":100,"device_type":"HB"},
            {"device_id":200,"device_type":"ST"}]}],"status":{"status_code":0}}"#;
        assert_eq!(tempest_device(reply).unwrap(), 200);
    }

    #[test]
    fn an_account_without_a_tempest_has_nothing_to_stream() {
        let reply = r#"{"stations":[{"devices":[{"device_id":100,"device_type":"HB"}]}]}"#;
        assert!(tempest_device(reply).is_err());
        assert!(tempest_device(r#"{"stations":[]}"#).is_err());
    }
}
