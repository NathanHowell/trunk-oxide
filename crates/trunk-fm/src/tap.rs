//! What an analog channel runs on its audio, beside the gate: decoders that
//! hear in-band signalling and say what they heard.

use crate::signalling::Heard;

/// A decoder of in-band signalling.
///
/// It is fed the channel's raw 16 kHz audio (ahead of the recording's
/// band-pass, so the tones and bursts are in it) in blocks of any size, and
/// the result must not depend on the block sizes.
pub trait AudioTap: Send {
    /// Feeds `audio`, appending whatever is whole by its end. A [`Heard`]'s
    /// span counts the samples fed since the last [`Self::reset`].
    fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>);

    /// Forget the audio so far, as for a retuned channel.
    fn reset(&mut self);
}
