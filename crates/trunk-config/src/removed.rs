//! Options that no longer do anything. A config that still sets one is
//! refused with what replaced it (or why it went), rather than silently
//! ignored.

/// `(path, why)`: a dotted path into the TOML, `*` for any array element or
/// table key.
const REMOVED: &[(&str, &str)] = &[
    (
        "recorder.debug",
        "the UDP debug recorder went with GNU Radio",
    ),
    (
        "recorder.soft_vocoder",
        "it chose op25's DMR vocoder; DMR is not ported (trunk-oxide-map)",
    ),
    (
        "source.*.digital_recorders",
        "channels are shared by every source: set [recorder] max_channels (trunk-solver sizes it)",
    ),
    (
        "source.*.analog_recorders",
        "channels are shared by every source: set [recorder] max_channels (trunk-solver sizes it)",
    ),
    (
        "source.*.sigmf_recorders",
        "SigMF recording went with GNU Radio",
    ),
    (
        "source.*.silence_frames",
        "it was a GNU Radio source option",
    ),
    ("source.*.antenna", "an RTL-SDR has no antenna selection"),
    (
        "source.*.gain_settings",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.if_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.bb_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.mix_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.lna_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.tia_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.pga_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.amp_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.vga_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.vga1_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "source.*.vga2_gain",
        "an RTL-SDR has one tuner gain: set gain (or auto_gain)",
    ),
    (
        "system.*.deemphasis_tau",
        "trunk-fm de-emphasises at the standard 75 µs",
    ),
    (
        "system.*.filter_width",
        "an analog channel's filter is max_dev wide",
    ),
    (
        "system.*.talkgroup_display_format",
        "it formatted trunk-recorder's console",
    ),
    ("system.*.unit_tag", "unit tags were trunk-recorder's"),
    ("system.*.unit_tags", "unit tags were trunk-recorder's"),
    (
        "system.*.multi_site_system_number",
        "multi-site matching uses multi_site_system_name",
    ),
    (
        "system.*.decode_mdc",
        "analog signalling (MDC, FleetSync, STAR, TPS) is not decoded",
    ),
    (
        "system.*.decode_fsync",
        "analog signalling (MDC, FleetSync, STAR, TPS) is not decoded",
    ),
    (
        "system.*.decode_star",
        "analog signalling (MDC, FleetSync, STAR, TPS) is not decoded",
    ),
    (
        "system.*.decode_tps",
        "analog signalling (MDC, FleetSync, STAR, TPS) is not decoded",
    ),
    (
        "system.*.bandplan_base",
        "Smartnet is not ported (trunk-oxide-a2m)",
    ),
    (
        "system.*.bandplan_high",
        "Smartnet is not ported (trunk-oxide-a2m)",
    ),
    (
        "system.*.bandplan_spacing",
        "Smartnet is not ported (trunk-oxide-a2m)",
    ),
    (
        "system.*.bandplan_offset",
        "Smartnet is not ported (trunk-oxide-a2m)",
    ),
];

/// Remove every removed option from `root` (an old config being
/// migrated, a capture's say); returns what went, as [`find`] says it.
pub fn strip(root: &mut toml::Value) -> Vec<String> {
    let found = find(root);
    for f in &found {
        let path: Vec<&str> = f.split(':').next().unwrap_or_default().split('.').collect();
        remove(root, &path);
    }
    found
}

fn remove(v: &mut toml::Value, path: &[&str]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    match v {
        toml::Value::Table(t) if rest.is_empty() => {
            t.remove(*first);
        }
        toml::Value::Table(t) => {
            if let Some(child) = t.get_mut(*first) {
                remove(child, rest);
            }
        }
        toml::Value::Array(a) => {
            if let Some(child) = first.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                remove(child, rest);
            }
        }
        _ => {}
    }
}

/// Every removed option `root` sets, as `path: why`.
pub fn find(root: &toml::Value) -> Vec<String> {
    let mut found = Vec::new();
    for (pattern, why) in REMOVED {
        let segments: Vec<&str> = pattern.split('.').collect();
        walk(root, &segments, String::new(), &mut |path| {
            found.push(format!("{path}: {why}"));
        });
    }
    found
}

fn walk(v: &toml::Value, segments: &[&str], path: String, hit: &mut impl FnMut(String)) {
    let Some((first, rest)) = segments.split_first() else {
        hit(path);
        return;
    };
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    match (v, *first) {
        (toml::Value::Table(t), "*") => {
            for (k, child) in t {
                walk(child, rest, join(k), hit);
            }
        }
        (toml::Value::Array(a), "*") => {
            for (i, child) in a.iter().enumerate() {
                walk(child, rest, join(&i.to_string()), hit);
            }
        }
        (toml::Value::Table(t), key) => {
            if let Some(child) = t.get(key) {
                walk(child, rest, join(key), hit);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_options_are_found_wherever_they_are_set() {
        let root: toml::Value = r#"
[recorder]
soft_vocoder = true
max_channels = 4

[[source]]
gain = 40.0

[[source]]
digital_recorders = 6
if_gain = 1.0

[system.smco]
decode_mdc = false
type = "p25"
"#
        .parse()
        .unwrap();
        let mut found = find(&root);
        found.sort();
        let paths: Vec<&str> = found.iter().map(|f| f.split(':').next().unwrap()).collect();
        assert_eq!(
            paths,
            [
                "recorder.soft_vocoder",
                "source.1.digital_recorders",
                "source.1.if_gain",
                "system.smco.decode_mdc",
            ]
        );
        assert!(find(&"[recorder]\nmax_channels = 4".parse().unwrap()).is_empty());
        let mut migrated = root.clone();
        assert_eq!(strip(&mut migrated).len(), 4);
        assert!(find(&migrated).is_empty());
        assert_eq!(migrated["recorder"]["max_channels"].as_integer(), Some(4));
        assert_eq!(migrated["system"]["smco"]["type"].as_str(), Some("p25"));
    }
}
