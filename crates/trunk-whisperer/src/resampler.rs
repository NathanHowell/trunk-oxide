use anyhow::Result;
use rubato::{
    Resampler as _, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

const OUTPUT_RATE: u32 = 16000;
const CHUNK_SIZE: usize = 1024;

pub struct Resampler {
    inner: SincFixedIn<f32>,
    input_buffer: Vec<f32>,
}

impl Resampler {
    pub fn new(input_rate: u32) -> Result<Self> {
        let ratio = f64::from(OUTPUT_RATE) / f64::from(input_rate);
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = SincFixedIn::new(ratio, 2.0, params, CHUNK_SIZE, 1)?;
        Ok(Self {
            inner,
            input_buffer: Vec::new(),
        })
    }

    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        self.input_buffer.extend_from_slice(input);
        let mut output = Vec::new();
        let needed = self.inner.input_frames_next();

        while self.input_buffer.len() >= needed {
            let chunk: Vec<f32> = self.input_buffer.drain(..needed).collect();
            let result = self.inner.process(&[&chunk], None)?;
            output.extend_from_slice(&result[0]);
        }

        Ok(output)
    }

    pub fn flush(&mut self) -> Result<Vec<f32>> {
        if self.input_buffer.is_empty() {
            return Ok(Vec::new());
        }
        let chunk: Vec<f32> = self.input_buffer.drain(..).collect();
        let result = self.inner.process_partial(Some(&[&chunk]), None)?;
        Ok(result[0].clone())
    }
}
