//! What a deployed config must get right that parsing cannot see.
//!
//! The sources' rates: `trunk-solver` plans them and the production entry
//! point refuses a config that does not follow. A replay's sources take the
//! rate they were recorded at, so it does not ask.

use anyhow::{Context as _, Result, ensure};
use trunk_config::SourceConfig;

/// Refuses an enabled source whose rate gives its channelizer a slow forward
/// FFT (a large prime stage; see [`trunk_radio::ChannelizerConfig::is_fast`]).
///
/// # Errors
/// On the first source at such a rate, or at one no channelizer can run.
pub fn check(sources: &[SourceConfig]) -> Result<()> {
    for src in sources.iter().filter(|s| s.enabled != Some(false)) {
        let geometry = trunk_radio::ChannelizerConfig::standard(src.rate)
            .with_context(|| format!("source '{}' at {} S/s", src.device, src.rate))?;
        ensure!(
            geometry.is_fast(),
            "source '{}': {} S/s gives a {}-point channelizer FFT with a large prime stage, several times the cost of a smooth size; plan the source with trunk-solver",
            src.device,
            src.rate,
            geometry.fft_size()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;
    use trunk_config::Config;

    fn sources(toml: &str) -> Vec<trunk_config::SourceConfig> {
        toml::from_str::<Config>(toml).unwrap().source
    }

    #[test]
    fn a_slow_fft_rate_is_refused() {
        // 2.472 MS/s is 103 x 24 kHz.
        let slow = "[[source]]\ndriver = \"rtlsdr\"\ndevice = \"rtl=1\"\nrate = 2472000.0\n";
        let err = check(&sources(slow)).unwrap_err().to_string();
        assert!(
            err.contains("rtl=1") && err.contains("30900-point"),
            "{err}"
        );
    }

    #[test]
    fn a_disabled_source_is_not_asked() {
        let slow = "[[source]]\ndriver = \"rtlsdr\"\nrate = 2472000.0\nenabled = false\n";
        assert!(check(&sources(slow)).is_ok());
    }

    #[test]
    fn a_smooth_rate_passes() {
        let fast = "[[source]]\ndriver = \"rtlsdr\"\nrate = 2496000.0\n[[source]]\ndriver = \"rtlsdr\"\nrate = 960000.0\n";
        assert!(check(&sources(fast)).is_ok());
    }
}
