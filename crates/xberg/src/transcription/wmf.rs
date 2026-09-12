//! Windows Media audio decoding through Media Foundation.
//!
//! Windows ships the WMV/WMA decoders and `IMFSourceReader` exposes them, which
//! keeps the container rescue path working without a second media stack in the
//! binary (an ffmpeg able to do this is >150 MB of DLLs). The reader delivers
//! PCM in the container's own rate/channel layout; the downmix and resample
//! helpers in [`super::decode`] then produce the 16 kHz mono the engine wants,
//! so no dependency on Media Foundation's optional audio-processing pipeline is
//! needed.
//!
//! Only the audio stream is read -- that is all the transcription pipeline
//! consumes. Non-Windows targets get a stub that reports the limitation.

use std::path::Path;

use crate::Result;
use crate::transcription::decode::PcmAudio;

#[cfg(target_os = "windows")]
mod imp {
    use std::path::Path;

    use windows::Win32::Media::MediaFoundation::{
        IMFMediaType, IMFSample, IMFSourceReader, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
        MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFAudioFormat_Float, MFAudioFormat_PCM, MFCreateMediaType,
        MFCreateSourceReaderFromURL, MFMediaType_Audio, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::HSTRING;

    use crate::Result;
    use crate::XbergError;
    use crate::transcription::decode::{PcmAudio, down_mix_to_mono, resample_linear_to_16k};

    /// Media Foundation session; shutdown runs on every exit path.
    struct Session;

    impl Session {
        #[allow(unsafe_code)]
        fn start() -> Result<Self> {
            // MF runs on COM. The extraction path is already multi-threaded, so
            // the multithreaded apartment is the right one; `RPC_E_CHANGED_MODE`
            // means this thread already lives in another apartment, which Media
            // Foundation tolerates.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                    .map_err(|e| XbergError::transcription(format!("Media Foundation could not start: {e}")))?;
            }
            Ok(Self)
        }
    }

    impl Drop for Session {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
        }
    }

    /// The PCM layout the reader agreed to deliver.
    #[derive(Clone, Copy)]
    struct PcmLayout {
        sample_rate: u32,
        channels: usize,
        bits: u32,
        float: bool,
    }

    /// Ask the reader for uncompressed PCM at the container's own rate/channels.
    ///
    /// Requesting 16 kHz mono directly would need Media Foundation's audio
    /// processing pipeline; converting locally instead keeps this path free of
    /// that dependency (and of the undocumented property key it needs).
    #[allow(unsafe_code)]
    fn configure_pcm_output(reader: &IMFSourceReader) -> Result<PcmLayout> {
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        unsafe {
            let native: IMFMediaType = reader
                .GetNativeMediaType(stream, 0)
                .map_err(|e| XbergError::transcription(format!("the media file has no audio track: {e}")))?;
            let sample_rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(44_100);
            let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(1) as usize;
            let bits = native.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16);

            let requested: IMFMediaType = MFCreateMediaType()
                .map_err(|e| XbergError::transcription(format!("Media Foundation media type: {e}")))?;
            requested
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .and_then(|()| requested.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM))
                .and_then(|()| requested.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16))
                .and_then(|()| requested.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate))
                .and_then(|()| requested.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32))
                .map_err(|e| XbergError::transcription(format!("Media Foundation PCM format: {e}")))?;
            reader.SetCurrentMediaType(stream, None, &requested).map_err(|e| {
                XbergError::transcription(format!("the media file's audio track cannot be converted to PCM: {e}"))
            })?;

            // What the reader actually settled on, which is what the bytes below
            // are interpreted as.
            let current: IMFMediaType = reader
                .GetCurrentMediaType(stream)
                .map_err(|e| XbergError::transcription(format!("Media Foundation media type: {e}")))?;
            let subtype = current.GetGUID(&MF_MT_SUBTYPE).unwrap_or(MFAudioFormat_PCM);
            Ok(PcmLayout {
                sample_rate: current
                    .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                    .unwrap_or(sample_rate),
                channels: current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(channels as u32) as usize,
                bits: current.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(bits),
                float: subtype == MFAudioFormat_Float,
            })
        }
    }

    /// Append one buffer's samples to `raw` in the layout's own encoding.
    #[allow(unsafe_code)]
    fn append_sample(sample: &IMFSample, layout: PcmLayout, raw: &mut Vec<f32>) -> Result<()> {
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| XbergError::transcription(format!("Media Foundation sample buffer: {e}")))?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut length))
                .map_err(|e| XbergError::transcription(format!("Media Foundation buffer lock: {e}")))?;

            let result = if data.is_null() || length == 0 {
                Ok(())
            } else if layout.float && layout.bits == 32 {
                let values = std::slice::from_raw_parts(data as *const f32, length as usize / 4);
                raw.extend_from_slice(values);
                Ok(())
            } else if !layout.float && layout.bits == 16 {
                let values = std::slice::from_raw_parts(data as *const i16, length as usize / 2);
                raw.extend(values.iter().map(|value| f32::from(*value) / 32_768.0));
                Ok(())
            } else {
                Err(XbergError::transcription(format!(
                    "unsupported PCM layout from Media Foundation: {}-bit {}",
                    layout.bits,
                    if layout.float { "float" } else { "integer" }
                )))
            };
            let _ = buffer.Unlock();
            result
        }
    }

    /// Decode the audio track of `path` into 16 kHz mono PCM.
    #[allow(unsafe_code)]
    pub(super) fn decode_file(path: &Path, max_bytes: Option<u64>) -> Result<PcmAudio> {
        let _session = Session::start()?;
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).map_err(|e| {
                XbergError::transcription(format!("Media Foundation cannot open the Windows Media input: {e}"))
            })?
        };
        let layout = configure_pcm_output(&reader)?;

        // Decoded-audio ceiling, not container size: 16 kHz mono f32 is 64 kB/s.
        let raw_limit = max_bytes.map(|bytes| (bytes as usize).saturating_mul(16));
        let mut raw: Vec<f32> = Vec::new();

        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                reader
                    .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .map_err(|e| XbergError::transcription(format!("Media Foundation read failed: {e}")))?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(sample) = sample {
                append_sample(&sample, layout, &mut raw)?;
            }
            if let Some(limit) = raw_limit
                && raw.len() > limit
            {
                return Err(XbergError::transcription(format!(
                    "decoded audio exceeds the transcription.max_bytes budget ({} bytes)",
                    max_bytes.unwrap_or_default()
                )));
            }
        }

        if raw.is_empty() {
            return Err(XbergError::transcription(
                "the media file's audio track decoded to no samples".to_string(),
            ));
        }

        let mono = down_mix_to_mono(&raw, layout.channels);
        let samples = if layout.sample_rate == 16_000 {
            mono
        } else {
            resample_linear_to_16k(&mono, layout.sample_rate)
        };
        let duration_ms = samples.len() as u64 * 1000 / 16_000;
        Ok(PcmAudio {
            samples,
            sample_rate_hz: 16_000,
            channels: 1,
            duration_ms,
        })
    }
}

/// Decode the audio track of a rescued container into 16 kHz mono PCM.
///
/// `max_bytes` mirrors `transcription.max_bytes`: it bounds the decoded audio,
/// not the container size (the caller already bounded the input).
pub(crate) fn decode_file_to_pcm(path: &Path, max_bytes: Option<u64>) -> Result<PcmAudio> {
    #[cfg(target_os = "windows")]
    {
        imp::decode_file(path, max_bytes)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (path, max_bytes);
        Err(crate::XbergError::transcription(
            "this build cannot read Windows Media (ASF/WMV) containers: decoding them needs Windows Media \
             Foundation. Convert the file to MP4/WebM/WAV first (for example with ffmpeg), or use the Windows build."
                .to_string(),
        ))
    }
}
