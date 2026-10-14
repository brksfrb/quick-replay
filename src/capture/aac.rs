//! AAC-LC encoding with the encoder built into Windows.

use super::mf;
use windows::core::{Result, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_SAMPLES: i64 = 1024;
const CLSID_AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
const AAC_LC_LEVEL: u32 = 0x29;

pub struct AacEncoder {
    transform: IMFTransform,
    output_size: u32,
}

/// The Windows encoder only accepts these average byte rates (96/128/160/192 kbps).
fn bytes_per_second(kbps: u32) -> u32 {
    match kbps {
        0..=111 => 12_000,
        112..=143 => 16_000,
        144..=175 => 20_000,
        _ => 24_000,
    }
}

impl AacEncoder {
    pub fn new(bitrate_kbps: u32) -> Result<AacEncoder> {
        let transform: IMFTransform = unsafe { CoCreateInstance(&CLSID_AAC_ENCODER, None, CLSCTX_INPROC_SERVER)? };
        let input = mf::media_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
        let output = mf::media_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
        unsafe {
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS * 2)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * CHANNELS * 2)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second(bitrate_kbps))?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw frames, no ADTS headers
            output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL)?;
            transform.SetInputType(0, &input, 0)?;
            transform.SetOutputType(0, &output, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        let output_size = unsafe { transform.GetOutputStreamInfo(0)?.cbSize }.max(8192);
        Ok(AacEncoder { transform, output_size })
    }

    /// Encodes interleaved stereo PCM; returns the AAC frames that came out (1024 samples each).
    pub fn encode(&mut self, pcm: &[i16], time: i64) -> Result<Vec<Vec<u8>>> {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let duration = pcm.len() as i64 / i64::from(CHANNELS) * super::clock::HNS_PER_SEC / i64::from(SAMPLE_RATE);
        let sample = mf::sample_from_bytes(&bytes, time, duration)?;
        unsafe { self.transform.ProcessInput(0, &sample, 0)? };
        let mut frames = Vec::new();
        loop {
            let out = mf::empty_sample(self.output_size)?;
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(out)),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
            let sample = std::mem::ManuallyDrop::into_inner(buffer.pSample);
            drop(std::mem::ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let data = mf::sample_bytes(&sample)?;
                        if !data.is_empty() {
                            frames.push(data);
                        }
                    }
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(err) => return Err(err),
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::bytes_per_second;

    #[test]
    fn maps_bitrates_to_supported_rates() {
        assert_eq!(bytes_per_second(96), 12_000);
        assert_eq!(bytes_per_second(128), 16_000);
        assert_eq!(bytes_per_second(160), 20_000);
        assert_eq!(bytes_per_second(192), 24_000);
    }
}
