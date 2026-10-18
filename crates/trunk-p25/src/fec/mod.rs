//! Forward error correction for voice frames: the binary block codes (Golay,
//! Hamming) and Reed–Solomon over GF(64).

pub mod binary;
pub mod rs;

pub use binary::{
    BinaryCode, BlockCode, Decision, GOLAY_18, GOLAY_23, GOLAY_24, HAMMING_10, HAMMING_15,
};
pub use rs::{RS_24_12, RS_24_16, RS_36_20, ReedSolomon, RsFailure};
