//! Settings trunk-oxide tunes for itself unless the config pins them.

use serde::de::{Deserialize, Deserializer, Error};

/// `"auto"`, the default (trunk-oxide measures and chooses), or a value that
/// pins the setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tunable<T> {
    #[default]
    Auto,
    Fixed(T),
}

impl<T: Copy> Tunable<T> {
    /// The pinned value, if the config pins one.
    pub const fn fixed(self) -> Option<T> {
        match self {
            Self::Auto => None,
            Self::Fixed(v) => Some(v),
        }
    }

    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Tunable<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw<T> {
            Word(String),
            Value(T),
        }
        match Raw::<T>::deserialize(d)? {
            Raw::Word(w) if w == "auto" => Ok(Self::Auto),
            Raw::Word(w) => Err(D::Error::custom(format!(
                "expected \"auto\" or a value, not \"{w}\""
            ))),
            Raw::Value(v) => Ok(Self::Fixed(v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct S {
        #[serde(default)]
        gain: Tunable<f64>,
    }

    fn gain(toml: &str) -> Result<Tunable<f64>, toml::de::Error> {
        toml::from_str::<S>(toml).map(|s| s.gain)
    }

    #[test]
    fn auto_unless_pinned() {
        assert_eq!(gain("").unwrap(), Tunable::Auto);
        assert_eq!(gain("gain = \"auto\"").unwrap(), Tunable::Auto);
        assert_eq!(gain("gain = 40.2").unwrap(), Tunable::Fixed(40.2));
        let e = gain("gain = \"max\"").unwrap_err().to_string();
        assert!(e.contains("\"auto\" or a value"), "{e}");
    }
}
