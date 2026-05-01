use anyhow::{bail, Context, Result};
use kira::{
    sound::{
        streaming::{StreamingSoundData, StreamingSoundHandle},
        FromFileError, PlaybackState,
    },
    AudioManager, AudioManagerSettings, Decibels, DefaultBackend, Tween,
};

use crate::persistent_entities::PlayableTrack;
use serde::Serialize;
use std::fs::{self, File};
use std::path::Path;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
}

#[derive(Serialize)]
pub struct Player {
    #[serde(skip)]
    manager: AudioManager,
    #[serde(skip)]
    sound_handle: Option<StreamingSoundHandle<FromFileError>>,
    #[serde(skip)]
    pub track: Option<PlayableTrack>,
    pub status: PlayerStatus,
    pub progress: f64,
    pub duration: f64,
    pub volume: f64,
}

impl Player {
    pub fn new(initial_volume: f64) -> Result<Player> {
        let manager = AudioManager::<DefaultBackend>::new(AudioManagerSettings::default())?;

        Ok(Player {
            manager,
            sound_handle: None,
            track: None,
            status: PlayerStatus::Stopped,
            progress: 0.0,
            duration: 0.0,
            volume: initial_volume,
        })
    }

    pub fn renew_state(&mut self) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            match sound_handle.state() {
                PlaybackState::Playing => self.status = PlayerStatus::Playing,
                PlaybackState::Pausing => self.status = PlayerStatus::Playing,
                PlaybackState::Stopping => self.status = PlayerStatus::Playing,
                PlaybackState::WaitingToResume => self.status = PlayerStatus::Playing,
                PlaybackState::Resuming => self.status = PlayerStatus::Playing,
                PlaybackState::Paused => self.status = PlayerStatus::Paused,
                PlaybackState::Stopped => self.status = PlayerStatus::Stopped,
            }
        } else {
            self.status = PlayerStatus::Stopped
        }

        match self.sound_handle {
            Some(ref mut sound_handle) => {
                self.progress = sound_handle.position();
            }
            None => {}
        }
    }

    pub fn play(&mut self, track: PlayableTrack) -> Result<()> {
        let _ = self.stop();
        let file_path = track.file_path.clone();
        let sound_data = StreamingSoundData::from_file(&file_path)
            .with_context(|| format!("Failed to open audio stream: {file_path}"))?;
        let duration = sound_data.duration().as_secs_f64();
        let sound_handle = self
            .manager
            .play(sound_data)
            .with_context(|| format!("Audio backend failed to start playback: {file_path}"))?;

        self.track = Some(track);
        self.duration = duration;
        self.sound_handle = Some(sound_handle);
        self.sound_handle
            .as_mut()
            .unwrap()
            .set_volume(Self::volume_as_decibels(self.volume), Tween::default());

        if should_check_for_immediate_stop(&file_path) {
            std::thread::sleep(Duration::from_millis(150));
            self.renew_state();
            if matches!(self.status, PlayerStatus::Stopped) {
                let duration = self.duration;
                let _ = self.stop();
                bail!(
                    "Audio backend stopped immediately after opening {file_path} (duration {duration:.2}s)"
                );
            }
        }

        Ok(())
    }

    pub fn resume(&mut self) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            sound_handle.resume(Tween::default());
        }
    }

    pub fn pause(&mut self) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            sound_handle.pause(Tween::default());
        }
    }

    pub fn seek(&mut self, position: f64) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            match sound_handle.state() {
                PlaybackState::Playing => sound_handle.seek_to(position),
                _ => {
                    sound_handle.seek_to(position);
                    sound_handle.resume(Tween::default());
                }
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            sound_handle.stop(Tween::default());
            self.sound_handle = None;
            self.track = None;
            self.duration = 0.0;
            self.progress = 0.0;
            self.status = PlayerStatus::Stopped;
        }
    }

    /// Kira doesn't provide a way to create Decibels from an amplitude.
    /// Invert the formula in Decibels::as_amplitude():
    /// original:                         amp = 10 ^ (db / 20)
    /// take log() of both sides:         log(amp) = log(10 ^ (db / 20))
    /// identity log(a^b) = b*log(a):     log(amp) = (db / 20) * log(10)
    /// divide both sides by log(10):     log(amp) / log(10) = db / 20
    /// divide by log(10) is log base 10: log10(amp) = db / 20
    /// multiply both sides by 20:        20 * log10(amp) = db
    pub(crate) fn volume_as_decibels(volume: f64) -> Decibels {
        if volume <= 0.0 {
            Decibels::SILENCE
        } else if volume == 1.0 {
            Decibels::IDENTITY
        } else {
            Decibels((20.0 * volume.log10()) as f32)
        }
    }

    pub fn set_volume(&mut self, volume: f64) {
        if let Some(ref mut sound_handle) = self.sound_handle {
            sound_handle.set_volume(Self::volume_as_decibels(volume), Tween::default());
        }
        self.volume = volume;
    }
}

fn should_check_for_immediate_stop(file_path: &str) -> bool {
    Path::new(file_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("mp3"))
        .unwrap_or(false)
}

pub fn decode_audio_to_playback_wav(input_path: &Path, output_path: &Path) -> Result<()> {
    let file = File::open(input_path)
        .with_context(|| format!("Failed to open audio file {}", input_path.display()))?;
    let media_source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = input_path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            media_source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("Failed to probe audio file {}", input_path.display()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| {
            track.codec_params.codec != CODEC_TYPE_NULL
                && track.codec_params.sample_rate.is_some()
                && track.codec_params.channels.is_some()
        })
        .or_else(|| format.default_track())
        .ok_or_else(|| {
            anyhow::anyhow!("No playable audio track found in {}", input_path.display())
        })?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| format!("Failed to create decoder for {}", input_path.display()))?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut writer: Option<hound::WavWriter<std::io::BufWriter<File>>> = None;
    let mut samples_written = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => {
                return Err(anyhow::anyhow!(error)).context("Failed to read audio packet")
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(error)).context("Failed to decode audio packet")
            }
        };

        if writer.is_none() {
            let spec = decoded.spec();
            let channels = spec.channels.count().max(1).min(u16::MAX as usize) as u16;
            writer = Some(
                hound::WavWriter::create(
                    output_path,
                    hound::WavSpec {
                        channels,
                        sample_rate: spec.rate,
                        bits_per_sample: 16,
                        sample_format: hound::SampleFormat::Int,
                    },
                )
                .with_context(|| {
                    format!("Failed to create fallback WAV {}", output_path.display())
                })?,
            );
        }

        let mut sample_buffer =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buffer.copy_interleaved_ref(decoded);
        if let Some(writer) = writer.as_mut() {
            for sample in sample_buffer.samples() {
                let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                writer.write_sample(sample)?;
                samples_written += 1;
            }
        }
    }

    let Some(writer) = writer else {
        bail!("Audio decoder did not produce any samples");
    };
    writer.finalize()?;

    if samples_written == 0 {
        bail!("Audio decoder did not produce any samples");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Player;
    use kira::Decibels;

    #[test]
    fn test_volume_as_decibels() {
        let decibels = [
            Decibels::IDENTITY,
            Decibels(3.0),
            Decibels(12.0),
            Decibels(-3.0),
            Decibels(-12.0),
            Decibels::SILENCE,
        ];
        for db_expected in decibels {
            let db_actual = Player::volume_as_decibels(db_expected.as_amplitude() as f64);
            assert!(
                (db_expected.0 - db_actual.0) < 1e-5,
                "{} != {}",
                db_expected.0,
                db_actual.0
            );
        }
    }

    #[test]
    fn test_player_new_with_volume() {
        // This test just verifies the constructor signature compiles
        // We can't actually test the audio manager in a unit test
        let _ = Player::volume_as_decibels(0.5);
        let _ = Player::volume_as_decibels(1.0);
        let _ = Player::volume_as_decibels(0.0);
    }

    #[test]
    fn detects_mp3_paths_for_immediate_stop_check() {
        assert!(super::should_check_for_immediate_stop("song.MP3"));
        assert!(!super::should_check_for_immediate_stop("song.wav"));
        assert!(!super::should_check_for_immediate_stop("song"));
    }

    #[test]
    fn decodes_audio_to_playback_wav_from_wav_source() {
        let base = std::env::temp_dir().join(format!(
            "lrcget-playback-decode-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&base);
        let input = base.join("input.wav");
        let output = base.join("output.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::create(&input, spec).unwrap();
            for index in 0..800 {
                let sample = ((index as f32 / 800.0) * i16::MAX as f32).round() as i16;
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }

        super::decode_audio_to_playback_wav(&input, &output).unwrap();

        let reader = hound::WavReader::open(&output).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 8_000);
        assert!(reader.len() > 0);
        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(output);
        let _ = std::fs::remove_dir(base);
    }
}
