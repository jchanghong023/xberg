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
    struct Session {
        /// Whether `CoInitializeEx` succeeded on this thread -- only then does
        /// this thread owe a matching `CoUninitialize`.
        com_initialized: bool,
        /// Whether `MFStartup` succeeded -- only then does this thread owe a
        /// matching `MFShutdown`.
        mf_started: bool,
    }

    impl Session {
        #[allow(unsafe_code)]
        fn start() -> Result<Self> {
            // MF runs on COM. The extraction path is already multi-threaded, so
            // the multithreaded apartment is the right one; `RPC_E_CHANGED_MODE`
            // means this thread already lives in another apartment, which Media
            // Foundation tolerates -- but that call added no COM reference of
            // ours, so it must not be released in `Drop`. The guard exists
            // before `MFStartup` so a failing startup still runs this cleanup.
            let mut session = Self {
                com_initialized: unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() },
                mf_started: false,
            };
            unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                    .map_err(|e| XbergError::transcription(format!("Media Foundation could not start: {e}")))?;
            }
            session.mf_started = true;
            Ok(session)
        }
    }

    impl Drop for Session {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            unsafe {
                if self.mf_started {
                    let _ = MFShutdown();
                }
                if self.com_initialized {
                    CoUninitialize();
                }
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

    /// Decoded duration of `raw` in milliseconds, from the layout's own frame
    /// rate. The reader delivers interleaved frames, so the sample count is the
    /// exact decoded length -- no per-sample COM query is needed.
    fn decoded_duration_ms(raw: &[f32], layout: PcmLayout) -> u64 {
        let channels = layout.channels.max(1) as u64;
        let frame_rate = u64::from(layout.sample_rate.max(1));
        (raw.len() as u64 / channels) * 1000 / frame_rate
    }

    /// Consecutive `ReadSample` calls that may deliver no new sample before the
    /// stream is treated as stalled.
    const MAX_IDLE_READS: usize = 64;

    /// Decode the audio track of `path` into 16 kHz mono PCM.
    #[allow(unsafe_code)]
    pub(super) fn decode_file(path: &Path, max_bytes: Option<u64>, max_duration_ms: Option<u64>) -> Result<PcmAudio> {
        let _session = Session::start()?;
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).map_err(|e| {
                XbergError::transcription(format!("Media Foundation cannot open the Windows Media input: {e}"))
            })?
        };
        let layout = configure_pcm_output(&reader)?;

        // Decoded-audio ceiling: the input byte budget, reused as a sample count
        // (16 kHz mono f32 is 64 kB/s, so the default 512 MB still admits hours of
        // audio while rejecting a pathological stream). `transcription.max_duration_ms`
        // is applied below while the stream decodes, so neither a stream that never
        // ends nor one that outlasts the limit can hold this thread.
        let raw_limit = max_bytes.map(|bytes| bytes as usize);
        let mut raw: Vec<f32> = Vec::new();
        // Reads that delivered no new sample. A reader that keeps succeeding
        // without samples and without the end-of-stream flag would otherwise spin
        // here forever, holding one of the caller's blocking threads.
        let mut idle_reads = 0usize;

        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                reader
                    .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .map_err(|e| XbergError::transcription(format!("Media Foundation read failed: {e}")))?;
            }
            // Appended before the end-of-stream test: one call may deliver the
            // last sample and the end-of-stream flag together.
            let samples_before = raw.len();
            if let Some(sample) = sample {
                append_sample(&sample, layout, &mut raw)?;
            }
            if raw.len() == samples_before {
                idle_reads += 1;
                if idle_reads >= MAX_IDLE_READS {
                    return Err(XbergError::transcription(format!(
                        "Media Foundation produced no audio samples for {MAX_IDLE_READS} consecutive reads \
                         without reaching the end of the stream"
                    )));
                }
            } else {
                idle_reads = 0;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(limit) = raw_limit
                && raw.len() > limit
            {
                return Err(XbergError::transcription(format!(
                    "decoded audio exceeds the transcription.max_bytes budget ({} bytes)",
                    max_bytes.unwrap_or_default()
                )));
            }
            if let Some(limit_ms) = max_duration_ms
                && decoded_duration_ms(&raw, layout) > limit_ms
            {
                return Err(XbergError::transcription(format!(
                    "decoded audio exceeds the transcription.max_duration_ms budget ({limit_ms} ms)"
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
/// `max_duration_ms` mirrors `transcription.max_duration_ms` and stops the read
/// loop once that much audio has been decoded, instead of only rejecting the
/// result after the whole stream has been read.
pub(crate) fn decode_file_to_pcm(
    path: &Path,
    max_bytes: Option<u64>,
    max_duration_ms: Option<u64>,
) -> Result<PcmAudio> {
    #[cfg(target_os = "windows")]
    {
        imp::decode_file(path, max_bytes, max_duration_ms)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (path, max_bytes, max_duration_ms);
        Err(crate::XbergError::transcription(
            "this build cannot read Windows Media (ASF/WMV) containers: decoding them needs Windows Media \
             Foundation. Convert the file to MP4/WebM/WAV first (for example with ffmpeg), or use the Windows build."
                .to_string(),
        ))
    }
}
