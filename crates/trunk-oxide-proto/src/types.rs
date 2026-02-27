use std::fmt;

macro_rules! newtype_int {
    ($name:ident, $inner:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

newtype_int!(Talkgroup, i64);
newtype_int!(SysNum, i32);
newtype_int!(CallNum, i64);
newtype_int!(SourceId, i64);
newtype_int!(RecorderId, i32);
newtype_int!(CallRowId, i64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Freq(pub f64);

impl fmt::Display for Freq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4} MHz", self.0 / 1_000_000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct SignalDb(pub f64);

impl fmt::Display for SignalDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} dB", self.0)
    }
}
