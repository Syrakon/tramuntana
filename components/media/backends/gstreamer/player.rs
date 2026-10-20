/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};
use std::time;

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    PlaybackState, Player, PlayerError, PlayerEvent, SeekLock, SeekLockMsg, SourceBufferId,
    SourceBufferState, StreamType, VideoPlaybackQuality,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_source::MediaSource;
use crate::media_source_src::{ServoMediaSourceSrc, register_servo_media_source_src};
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::reaper;
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;
/// Decoding threads of the software AV1 decoder.
const DAV1D_THREADS: u32 = 2;
/// Frames the software AV1 decoder keeps in flight.
const DAV1D_FRAME_DELAY: i64 = 1;
/// Worker threads of the libav video decoders (`avdec_h264` and its siblings).
const AVDEC_THREADS: i32 = 2;

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let dur = media_info.duration();
    let duration = if let Some(dur) = dur {
        let mut nanos = dur.nseconds();
        nanos %= 1_000_000_000;
        let seconds = dur.seconds();
        Some(time::Duration::new(seconds, nanos as u32))
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
    MediaSource(ServoMediaSourceSrc),
}

/// Counts the video frames handed to the renderer.
#[derive(Default)]
struct ShownFrames {
    count: AtomicU64,
    /// The presentation time of the frame counted last. A frame comes twice, as the
    /// preroll and as the first sample, when playback starts or follows a seek.
    last: Mutex<Option<gstreamer::ClockTime>>,
}

impl ShownFrames {
    fn count(&self, presentation_time: Option<gstreamer::ClockTime>) {
        let Ok(mut last) = self.last.lock() else {
            return;
        };
        if presentation_time.is_some() && *last == presentation_time {
            return;
        }
        *last = presentation_time;
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

struct PlayerInner {
    player: gstreamer_play::Play,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    shown_frames: Arc<ShownFrames>,
    /// The frames the video sink dropped before it last started its count again, and
    /// the count it gave last.
    dropped_frames: Cell<(u64, u64)>,
    input_size: u64,
    seekable: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
    /// The source buffers, if the stream type is StreamType::MediaSource.
    media_source: MediaSource,
    /// Whether the pipeline has been asked to play the source buffers. Until
    /// then it has no tracks to start with.
    media_source_started: bool,
}

impl PlayerInner {
    pub fn video_playback_quality(&self) -> VideoPlaybackQuality {
        let dropped_now = self
            .video_sink
            .property::<gstreamer::Structure>("stats")
            .get::<u64>("dropped")
            .unwrap_or_default();
        // The sink starts its count again after a flush, which a seek causes. A count
        // lower than the last one tells that; frames dropped between the last call and
        // the flush are missed.
        let (mut dropped_earlier, dropped_last) = self.dropped_frames.get();
        if dropped_now < dropped_last {
            dropped_earlier += dropped_last;
        }
        self.dropped_frames.set((dropped_earlier, dropped_now));

        let dropped_frames = dropped_earlier + dropped_now;
        VideoPlaybackQuality {
            total_frames: self.shown_frames.count.load(Ordering::Relaxed) + dropped_frames,
            dropped_frames,
        }
    }

    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        if self.muted.get() == muted {
            return Ok(());
        }

        self.muted.set(muted);
        self.player.set_mute(muted);
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if self.stream_type == StreamType::Stream {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.player.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        if !self.paused.get() {
            return Ok(());
        }

        self.paused.set(false);
        self.can_resume.set(false);
        if self.is_waiting_for_media_source() {
            return Ok(());
        }
        self.player.play();
        Ok(())
    }

    /// The pipeline cannot be started before the source buffers have their tracks.
    fn is_waiting_for_media_source(&self) -> bool {
        self.stream_type == StreamType::MediaSource && !self.media_source_started
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        self.player.stop();
        self.paused.set(true);
        self.can_resume.set(false);
        self.last_metadata = None;
        self.source = None;
        self.media_source.stop();
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }

        self.paused.set(true);
        self.can_resume.set(true);
        if self.is_waiting_for_media_source() {
            return Ok(());
        }
        self.player.pause();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => {
                if let PlayerSource::Seekable(source) = source {
                    source
                        .push_end_of_stream()
                        .map(|_| ())
                        .map_err(|_| PlayerError::EOSFailed)
                } else {
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if self.stream_type == StreamType::Stream {
            return Err(PlayerError::NonSeekableStream);
        }
        if self.stream_type == StreamType::MediaSource {
            self.media_source.seek(time);
            if self.is_waiting_for_media_source() {
                return Ok(());
            }
        }
        if let Some(ref metadata) = self.last_metadata &&
            let Some(ref duration) = metadata.duration &&
            duration < &time::Duration::new(time as u64, 0)
        {
            gstreamer::warning!(self.cat, obj = &self.player, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        self.player
            .seek(gstreamer::ClockTime::from_nseconds(time as u64));
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        self.player.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        if self.stream_type == StreamType::MediaSource {
            return self.media_source.buffered();
        }

        let mut buffered_ranges = vec![];

        let Some(duration) = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
        else {
            return buffered_ranges;
        };

        let pipeline = self.player.pipeline();
        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        if self.stream_type == StreamType::MediaSource {
            // <https://w3c.github.io/media-source/#htmlmediaelement-extensions-seekable>
            return match self.media_source.duration() {
                Some(duration) => vec![Range {
                    start: 0.0,
                    end: duration,
                }],
                None => self.media_source.buffered(),
            };
        }

        // if the servosrc is seekable, we should return the duration of the media
        if let Some(metadata) = self.last_metadata.as_ref() &&
            metadata.is_seekable &&
            let Some(duration) = metadata.duration
        {
            return vec![Range {
                start: 0.0,
                end: duration.as_secs_f64(),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .player
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_video_track_enabled(enabled);
        Ok(())
    }

    fn media_source(&mut self) -> Result<&mut MediaSource, PlayerError> {
        if self.stream_type != StreamType::MediaSource {
            return Err(PlayerError::MediaSourceUnsupported);
        }
        Ok(&mut self.media_source)
    }

    fn add_source_buffer(&mut self, media_type: &str) -> Result<SourceBufferId, PlayerError> {
        if self.media_source_started {
            // The pipeline has been given its tracks already.
            return Err(PlayerError::QuotaExceeded);
        }
        self.media_source()?.add_source_buffer(media_type)
    }

    fn remove_source_buffer(&mut self, id: SourceBufferId) -> Result<(), PlayerError> {
        self.media_source()?.remove_source_buffer(id)
    }

    fn change_source_buffer_type(
        &mut self,
        id: SourceBufferId,
        media_type: &str,
    ) -> Result<(), PlayerError> {
        self.media_source()?.change_type(id, media_type)
    }

    fn append_data(
        &mut self,
        id: SourceBufferId,
        data: Vec<u8>,
    ) -> Result<SourceBufferState, PlayerError> {
        self.media_source()?.append(id, data)
    }

    fn reset_source_buffer_parser(&mut self, id: SourceBufferId) -> Result<(), PlayerError> {
        self.media_source()?.reset_parser(id)
    }

    fn remove_source_buffer_range(
        &mut self,
        id: SourceBufferId,
        range: Range<f64>,
    ) -> Result<SourceBufferState, PlayerError> {
        self.media_source()?.remove_range(id, range)
    }

    fn set_source_buffer_timestamp_offset(
        &mut self,
        id: SourceBufferId,
        offset: f64,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_timestamp_offset(id, offset)
    }

    fn set_source_buffer_sequence_mode(
        &mut self,
        id: SourceBufferId,
        sequence: bool,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_sequence_mode(id, sequence)
    }

    fn set_source_buffer_append_window(
        &mut self,
        id: SourceBufferId,
        window: Range<f64>,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_append_window(id, window)
    }

    fn start_media_source(&mut self) -> Result<(), PlayerError> {
        self.media_source()?;
        if self.media_source_started {
            return Ok(());
        }
        self.media_source_started = true;
        if self.paused.get() {
            self.player.pause();
        } else {
            self.player.play();
        }
        Ok(())
    }

    fn set_media_source_duration(&mut self, duration: Option<f64>) -> Result<(), PlayerError> {
        self.media_source()?.set_duration(duration);
        Ok(())
    }

    fn set_media_source_ended(&mut self, ended: bool) -> Result<(), PlayerError> {
        self.media_source()?.set_ended(ended);
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

/// A strong reference to the inner player for the length of a callback of its pipeline.
///
/// Such a callback runs on a thread of the player. If its reference were the last one, the
/// player would be destroyed on that thread, which must not happen: see `reaper`. So the
/// reference is let go of on the reaper thread.
struct Lease(Option<Arc<Mutex<PlayerInner>>>);

impl Lease {
    fn take(inner: &Weak<Mutex<PlayerInner>>) -> Option<Self> {
        inner.upgrade().map(|inner| Lease(Some(inner)))
    }

    fn lock(&self) -> Option<MutexGuard<'_, PlayerInner>> {
        self.0.as_ref()?.lock().ok()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            reaper::let_go(Box::new(inner));
        }
    }
}

/// The player while `setup` is at work on it. If `setup` gives up, the player goes to the
/// reaper thread together with the messages it has posted by then.
struct Unfinished(Option<gstreamer_play::Play>);

impl Unfinished {
    fn player(&self) -> Result<&gstreamer_play::Play, PlayerError> {
        self.0
            .as_ref()
            .ok_or_else(|| PlayerError::Backend("The player is already set up".to_owned()))
    }

    fn finish(mut self) -> Result<gstreamer_play::Play, PlayerError> {
        self.0
            .take()
            .ok_or_else(|| PlayerError::Backend("The player is already set up".to_owned()))
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some(player) = self.0.take() {
            let bus = player.message_bus();
            let weak_player = player.downgrade();
            reaper::let_go_of_player(Box::new(player), bus, weak_player);
        }
    }
}

/// Where an error of the pipeline goes while `setup` waits for the source to ask for data.
type SetupError = Mutex<Option<Sender<Result<(), PlayerError>>>>;

/// Handles a message of the player. It runs on the thread that posts the message.
fn handle_message(
    message: &gstreamer::Message,
    weak_inner: &Weak<Mutex<PlayerInner>>,
    observer: &Arc<Mutex<GenericCallback<PlayerEvent>>>,
    setup_error: &SetupError,
) {
    let Ok(play_message) = gstreamer_play::PlayMessage::parse(message) else {
        return;
    };
    match play_message {
        gstreamer_play::PlayMessage::EndOfStream(_) => {
            let _ = notify!(observer, PlayerEvent::EndOfStream);
        },
        gstreamer_play::PlayMessage::Error(error) => {
            let error = error.error().to_string();
            let _ = notify!(observer, PlayerEvent::Error(error.clone()));

            let waiting = setup_error.lock().ok().and_then(|sender| sender.clone());
            if let Some(sender) = waiting {
                let _ = sender.send(Err(PlayerError::Backend(error)));
                let player = message
                    .src()
                    .and_then(|src| src.downcast_ref::<gstreamer_play::Play>());
                if let Some(player) = player {
                    player.stop();
                }
            }
        },
        gstreamer_play::PlayMessage::StateChanged(state_changed) => {
            let play_state = state_changed.state();
            let Some(lease) = Lease::take(weak_inner) else {
                return;
            };
            if let Some(mut inner) = lease.lock() {
                inner.play_state = play_state;
            }

            let state = match play_state {
                gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                _ => None,
            };
            if let Some(v) = state {
                let _ = notify!(observer, PlayerEvent::StateChanged(v));
            }
        },
        gstreamer_play::PlayMessage::PositionUpdated(position_updated) => {
            if let Some(seconds) = position_updated.position().map(|p| p.seconds_f64()) {
                let Some(lease) = Lease::take(weak_inner) else {
                    return;
                };
                if let Some(inner) = lease.lock() {
                    inner.media_source.set_position(seconds);
                }
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        },
        gstreamer_play::PlayMessage::SeekDone(seek_done) => {
            if let Some(position) = seek_done.position() {
                let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
            }
        },
        gstreamer_play::PlayMessage::MediaInfoUpdated(media_info_updated) => {
            let Ok(metadata) = metadata_from_media_info(media_info_updated.media_info()) else {
                return;
            };

            let Some(lease) = Lease::take(weak_inner) else {
                return;
            };
            let Some(mut inner) = lease.lock() else {
                return;
            };

            if inner.last_metadata.as_ref() == Some(&metadata) {
                return;
            }

            // TODO: Workaround to generate expected `paused` state change event.
            // <https://github.com/servo/servo/issues/40740>
            let mut send_pause_event = false;

            if inner.last_metadata.is_none() {
                if metadata.is_seekable && inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE {
                    // The `paused` state change event will be fired after the
                    // seek initiated by the playback rate change has
                    // completed.
                    inner.player.set_rate(inner.playback_rate.get());
                } else if inner.play_state == gstreamer_play::PlayState::Paused {
                    // We need to send the paused event for non seekable push streams.
                    send_pause_event = true;
                }
            }

            inner.last_metadata = Some(metadata.clone());
            gstreamer::info!(
                inner.cat,
                obj = &inner.player,
                "Metadata updated: {:?}",
                metadata
            );
            let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata));

            if send_pause_event {
                let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
            }
        },
        gstreamer_play::PlayMessage::DurationChanged(duration_changed) => {
            let duration = duration_changed.duration().map(|duration| {
                time::Duration::new(
                    duration.seconds(),
                    (duration.nseconds() % 1_000_000_000) as u32,
                )
            });

            let Some(lease) = Lease::take(weak_inner) else {
                return;
            };
            let Some(mut inner) = lease.lock() else {
                return;
            };
            if let Some(ref mut metadata) = inner.last_metadata &&
                metadata.duration != duration
            {
                metadata.duration = duration;
                gstreamer::info!(
                    inner.cat,
                    obj = &inner.player,
                    "Duration changed: {:?}",
                    duration
                );
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        },
        _ => {},
    }
}

struct SeekChannel {
    sender: SeekLock,
    recv: GenericReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) =
            generic_channel::channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> SeekLockMsg {
        self.recv.recv().unwrap()
    }
}

pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Decorator used to setup the video sink and process the produced frames.
    render: Arc<Mutex<GStreamerRender>>,
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer: Arc::new(Mutex::new(observer)),
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
        }
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have the elements that we
        // need to make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            if gstreamer::ElementFactory::find(element).is_none() {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        let unfinished = Unfinished(Some(gstreamer_play::Play::default()));
        let player = unfinished.player()?;
        let pipeline = player.pipeline();

        // The progressive downloading of playbin is not used: its download buffer cannot give
        // the end of the media resource, which is looked at to find its type, before it has
        // all of it, so nothing could be played or even known about the media before the
        // download was over. The data is fetched again if it is ever needed again.

        // Set max size for the player buffer.
        pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);

        // The volume and mute of a media element are applied inside the pipeline,
        // not on the stream of the audio server. PipeWire and PulseAudio remember
        // the volume and mute of a stream by the name of the application and give
        // them to every stream it opens afterwards: a page that mutes one video
        // would leave the next one silent while its own state says it is not muted.
        let flags = pipeline.property_value("flags");
        let flags = glib::FlagsClass::with_type(flags.type_())
            .and_then(|class| {
                class
                    .builder_with_value(flags)
                    .and_then(|builder| builder.set_by_nick("soft-volume").build())
            })
            .ok_or_else(|| {
                PlayerError::Backend("playbin does not take the soft-volume flag".to_owned())
            })?;
        pipeline.set_property_from_value("flags", &flags);

        // The flag alone does not keep the audio server out of it: measured with
        // PipeWire, a page that mutes its video still mutes the stream, the server
        // remembers that, and the next stream of the application is born muted and
        // at the remembered volume while its page says neither. Stating both here
        // makes the sink open its stream with them instead of with what the server
        // remembers. They are the values `PlayerInner` starts from.
        player.set_mute(DEFAULT_MUTED);
        player.set_volume(DEFAULT_VOLUME);

        // Set player position interval update to 0.5 seconds. With source buffers
        // the position is also what tells that playback is about to run out of
        // data, which has to be noticed sooner.
        let mut config = player.config();
        config.set_position_update_interval(match self.stream_type {
            StreamType::MediaSource => 100u32,
            _ => 500u32,
        });
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        // The software AV1 decoder, which lib.rs puts ahead of the hardware ones, starts one
        // thread per logical core by default and keeps as many frames in flight, each with
        // its own picture buffers. Measured on a YouTube video in a content process, two
        // threads and one frame in flight make the process about 60 MB smaller than the
        // defaults and take less CPU, and playback keeps up. Both properties only take
        // effect before the decoder leaves the READY state, which holds when it is added.
        //
        // The libav decoders, which lib.rs puts ahead of NVDEC, do the same with frame
        // threading: one thread per logical core plus one, each holding the frames it decodes.
        // On a Twitch 1080p H.264 stream with sixteen cores that was twelve more threads in
        // the content process than with NVDEC.
        if let Some(bin) = pipeline.downcast_ref::<gstreamer::Bin>() {
            bin.connect_deep_element_added(|_, _, element| {
                let Some(factory) = element.factory() else {
                    return;
                };
                // set_property panics on a missing property or a value of another type, so
                // each one is checked against the decoder that is actually installed.
                let takes = |name: &str, value_type: glib::Type| {
                    element
                        .find_property(name)
                        .is_some_and(|spec| spec.value_type() == value_type)
                };
                if factory.name() == "dav1ddec" {
                    if takes("n-threads", glib::Type::U32) {
                        element.set_property("n-threads", DAV1D_THREADS);
                    }
                    if takes("max-frame-delay", glib::Type::I64) {
                        element.set_property("max-frame-delay", DAV1D_FRAME_DELAY);
                    }
                } else if factory.name().starts_with("avdec_") &&
                    factory.has_type(
                        gstreamer::ElementFactoryType::DECODER |
                            gstreamer::ElementFactoryType::MEDIA_VIDEO,
                    ) &&
                    takes("max-threads", glib::Type::I32)
                {
                    element.set_property("max-threads", AVDEC_THREADS);
                }
            });
        }

        if let Some(ref audio_renderer) = self.audio_renderer {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();

            let weak_audio_renderer = Arc::downgrade(audio_renderer);

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Err(gstreamer::FlowError::Flushing);
                        };

                        for position in positions.iter() {
                            let buffer = buffer.clone();
                            let map = match buffer.into_mapped_buffer_readable() {
                                Ok(map) => map,
                                _ => {
                                    return Err(gstreamer::FlowError::Error);
                                },
                            };
                            let chunk = Box::new(GStreamerAudioChunk(map));
                            let channel = position.to_mask() as u32;

                            audio_renderer.lock().unwrap().render(chunk, channel);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;
        let shown_frames = Arc::<ShownFrames>::default();

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
            StreamType::MediaSource => {
                register_servo_media_source_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediasourcesrc registration error: {error:?}"
                    ))
                })?;
                "servomediasource://".to_value()
            },
        };
        player.set_property("uri", &uri);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        let player = unfinished.finish()?;
        let bus = player.message_bus();
        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            player,
            source: None,
            video_sink,
            shown_frames: shown_frames.clone(),
            dropped_frames: Cell::new((0, 0)),
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
            media_source: MediaSource::default(),
            media_source_started: false,
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        // The handlers below belong to the bus and to the pipeline, which the inner player
        // owns. They hold it weakly: a strong reference would keep the player, its
        // pipeline and its source buffers alive for ever.
        let weak_inner = Arc::downgrade(inner);
        let setup_error = Arc::new(SetupError::default());
        // The messages are handled where they are posted, and stay on the bus for the
        // reaper thread. The ones posted before this were for nobody.
        bus.set_sync_handler({
            let weak_inner = weak_inner.clone();
            let observer = self.observer.clone();
            let setup_error = setup_error.clone();
            move |bus, message| {
                handle_message(message, &weak_inner, &observer, &setup_error);
                reaper::keep_message(bus)
            }
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);

                move |sample: gstreamer::Sample| {
                    let presentation_time = sample.buffer().and_then(|buffer| buffer.pts());
                    let Some(frame) = render.lock().unwrap().get_frame_from_sample(sample) else {
                        return Err(gstreamer::FlowError::Error);
                    };

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            video_renderer.lock().unwrap().render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                    shown_frames.count(presentation_time);
                    let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                    Ok(gstreamer::FlowSuccess::Ok)
                }
            };

            // Set video_sink callbacks.
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        move |video_sink| {
                            render_sample(
                                video_sink
                                    .pull_preroll()
                                    .map_err(|_| gstreamer::FlowError::Eos)?,
                            )
                        }
                    })
                    .new_sample(move |video_sink| {
                        render_sample(
                            video_sink
                                .pull_sample()
                                .map_err(|_| gstreamer::FlowError::Eos)?,
                        )
                    })
                    .build(),
            );
        };

        let receiver = {
            let inner_clone = weak_inner.clone();
            let inner = inner.lock().unwrap();
            let pipeline = inner.player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let error_sender = sender.clone();
            let sender = Arc::new(Mutex::new(sender));
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            pipeline.connect("source-setup", false, move |args| {
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let lease = Lease::take(&inner_clone)?;
                let mut inner = lease.lock()?;
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        // The source owns its callbacks, so they hold it weakly.
                        let servosrc_ = servosrc.downgrade();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    let Some(servosrc_) = servosrc_.upgrade() else {
                                        return false;
                                    };
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        let (ret, ack_channel) =
                                            seek_channel.lock().unwrap()._await();
                                        (ret, Some(ack_channel))
                                    } else {
                                        (true, None)
                                    };

                                    servosrc_.set_seek_done();
                                    if let Some(ack_channel) = ack_channel {
                                        ack_channel.send(()).unwrap();
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                    StreamType::MediaSource => {
                        let started = source
                            .dynamic_cast::<ServoMediaSourceSrc>()
                            .map_err(|_| {
                                "The source element is not a ServoMediaSourceSrc".to_owned()
                            })
                            .and_then(|media_source_src| {
                                inner
                                    .media_source
                                    .start(&media_source_src)
                                    .map_err(|error| error.to_string())?;
                                Ok(media_source_src)
                            });
                        match started {
                            Ok(media_source_src) => PlayerSource::MediaSource(media_source_src),
                            Err(error) => {
                                let _ = notify!(
                                    observer,
                                    PlayerEvent::Error(format!(
                                        "Could not set up the tracks of the source buffers: {error}"
                                    ))
                                );
                                return None;
                            },
                        }
                    },
                };

                inner.set_src(source);

                None
            });

            if inner.stream_type == StreamType::MediaSource {
                // The pipeline is started once the source buffers have their
                // tracks, and its errors are for the observer from the beginning.
                return Ok(());
            }

            // Until the source asks for data, an error of the pipeline ends the wait.
            if let Ok(mut setup_error) = setup_error.lock() {
                *setup_error = Some(error_sender);
            }

            inner.player.pause();

            receiver
        };

        let result = receiver.recv().unwrap();
        if let Ok(mut setup_error) = setup_error.lock() {
            *setup_error = None;
        }
        result
    }
}

impl GStreamerPlayer {
    /// Run `f` on the player, setting it up first.
    fn with_inner<T>(
        &self,
        f: impl FnOnce(&mut PlayerInner) -> Result<T, PlayerError>,
    ) -> Result<T, PlayerError> {
        self.setup()?;
        let inner = self.inner.borrow();
        let inner = inner
            .as_ref()
            .ok_or_else(|| PlayerError::Backend("The player is not set up".to_owned()))?;
        let mut inner = inner
            .lock()
            .map_err(|_| PlayerError::Backend("The player lock is poisoned".to_owned()))?;
        f(&mut inner)
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);
    inner_player_proxy!(stop, ());
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(
        video_playback_quality,
        VideoPlaybackQuality,
        VideoPlaybackQuality::default()
    );
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(remove_source_buffer, id, SourceBufferId);
    inner_player_proxy!(reset_source_buffer_parser, id, SourceBufferId);
    inner_player_proxy!(
        set_source_buffer_timestamp_offset,
        id,
        SourceBufferId,
        offset,
        f64
    );
    inner_player_proxy!(
        set_source_buffer_sequence_mode,
        id,
        SourceBufferId,
        sequence,
        bool
    );
    inner_player_proxy!(
        set_source_buffer_append_window,
        id,
        SourceBufferId,
        window,
        Range<f64>
    );
    inner_player_proxy!(start_media_source, ());
    inner_player_proxy!(set_media_source_duration, duration, Option<f64>);
    inner_player_proxy!(set_media_source_ended, ended, bool);

    fn add_source_buffer(&self, media_type: &str) -> Result<SourceBufferId, PlayerError> {
        self.with_inner(|inner| inner.add_source_buffer(media_type))
    }

    fn change_source_buffer_type(
        &self,
        id: SourceBufferId,
        media_type: &str,
    ) -> Result<(), PlayerError> {
        self.with_inner(|inner| inner.change_source_buffer_type(id, media_type))
    }

    fn append_data(
        &self,
        id: SourceBufferId,
        data: Vec<u8>,
    ) -> Result<SourceBufferState, PlayerError> {
        self.with_inner(|inner| inner.append_data(id, data))
    }

    fn remove_source_buffer_range(
        &self,
        id: SourceBufferId,
        range: Range<f64>,
    ) -> Result<SourceBufferState, PlayerError> {
        self.with_inner(|inner| inner.remove_source_buffer_range(id, range))
    }

    fn set_download_buffering_enabled(&self, _enable: bool) -> Result<(), PlayerError> {
        // Never enabled: see the note about progressive downloading in `setup`.
        self.setup()?;
        Ok(())
    }

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        // A player that was never set up has nothing to stop, and `stop` would set it up.
        if self.inner.borrow().is_some() {
            let _ = self.stop();
        }
        let (tx_ack, rx_ack) = mpsc::channel();
        let _ = self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            });
        let _ = rx_ack.recv();

        // The player is given up on the reaper thread: the last reference to it must not
        // be dropped on one of its own threads, and its pipeline takes a while to stop.
        if let Some(inner) = self.inner.borrow_mut().take() {
            let player = inner
                .lock()
                .ok()
                .map(|inner| (inner.player.message_bus(), inner.player.downgrade()));
            match player {
                Some((bus, weak_player)) => {
                    reaper::let_go_of_player(Box::new(inner), bus, weak_player)
                },
                None => reaper::let_go(Box::new(inner)),
            }
        }
    }
}
