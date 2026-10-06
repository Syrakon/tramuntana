/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions for the GStreamer player.
//!
//! Every source buffer owns a small pipeline, `appsrc ! demuxer ! appsink`, that
//! turns the appended bytes into coded frames. The frames are kept per track,
//! in decode order, and are what the buffered ranges are computed from. Once
//! playback starts every track gets an `appsrc` inside a
//! [`ServoMediaSourceSrc`], which is fed from the stored frames: from the
//! keyframe before the playback position after a seek, and in decode order from
//! there on.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound::{Excluded, Unbounded};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use gstreamer::prelude::*;
use mime::Mime;
use servo_media_player::{
    PlayerError, SourceBufferId, SourceBufferState, SourceBufferTrack, SourceBufferTrackKind,
};

use crate::media_source_src::ServoMediaSourceSrc;

const NANOSECONDS: f64 = 1_000_000_000.;

/// Offset given to the empty buffer that follows the bytes of an append. Seeing
/// it leave the appsrc means the demuxer is done with everything before it.
const END_OF_APPEND_MARKER: u64 = u64::MAX - 1;

/// How long an append may take before it is reported as failed.
const APPEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Buffered ranges closer than this are reported as one, and a position this
/// close to the start of a range counts as being inside of it.
const MAX_GAP: u64 = 250_000_000;

/// How much a track may hold before old frames are evicted.
const VIDEO_QUOTA: usize = 150 * 1024 * 1024;
const AUDIO_QUOTA: usize = 30 * 1024 * 1024;

/// Frames this far behind the playback position are the ones evicted first.
const EVICTION_MARGIN: u64 = 10_000_000_000;

/// How much the playback appsrc of a track queues before asking to stop.
const PLAYBACK_QUEUE_TIME: u64 = 2_000_000_000;
const PLAYBACK_QUEUE_BYTES: u64 = 16 * 1024 * 1024;

/// The state guarded by these mutexes stays consistent if a thread panics while
/// holding one of them, so a poisoned lock is still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn seconds(nanoseconds: u64) -> f64 {
    nanoseconds as f64 / NANOSECONDS
}

fn nanoseconds(seconds: f64) -> u64 {
    if seconds.is_nan() || seconds <= 0. {
        0
    } else if seconds >= u64::MAX as f64 / NANOSECONDS {
        u64::MAX
    } else {
        (seconds * NANOSECONDS).round() as u64
    }
}

fn intersect(a: &[Range<u64>], b: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].start.max(b[j].start);
        let end = a[i].end.min(b[j].end);
        if start < end {
            result.push(start..end);
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    result
}

/// State every track needs and the player changes.
#[derive(Default)]
struct Shared {
    /// No more data is going to be appended.
    ended: AtomicBool,
    /// Where playback is, in nanoseconds.
    position: AtomicU64,
}

struct Frame {
    pts: u64,
    end: u64,
    keyframe: bool,
    /// The demuxer gave no duration, so `end` is a guess until the next frame
    /// arrives.
    guessed_end: bool,
    caps: gstreamer::Caps,
    buffer: gstreamer::Buffer,
}

impl Frame {
    fn size(&self) -> usize {
        self.buffer.size()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Feed {
    /// Waiting for the frames at this position, to start from the keyframe
    /// before it.
    From(u64),
    /// Every frame up to this decode timestamp has been pushed.
    After(i64),
}

struct TrackBuffer {
    /// The coded frames, by decode timestamp.
    frames: BTreeMap<i64, Frame>,
    bytes: usize,
    /// Cache of the presentation ranges of `frames`.
    ranges: Option<Vec<Range<u64>>>,
    /// The last frame added by the append in progress.
    last_appended: Option<i64>,
    last_duration: u64,
    longest_duration: u64,
    /// Frames are being dropped until a keyframe arrives.
    need_keyframe: bool,
    info: SourceBufferTrack,
    caps: Option<gstreamer::Caps>,

    // Playback.
    appsrc: Option<gstreamer_app::AppSrc>,
    feed: Feed,
    /// Where the frames pushed so far end.
    feed_end: u64,
    pushed_any: bool,
    pushed_caps: Option<gstreamer::Caps>,
    eos_sent: bool,
}

impl TrackBuffer {
    fn new(kind: SourceBufferTrackKind) -> Self {
        Self {
            frames: BTreeMap::new(),
            bytes: 0,
            ranges: None,
            last_appended: None,
            last_duration: 0,
            longest_duration: 0,
            need_keyframe: true,
            info: SourceBufferTrack {
                kind,
                codec: String::new(),
                width: 0,
                height: 0,
            },
            caps: None,
            appsrc: None,
            feed: Feed::From(0),
            feed_end: 0,
            pushed_any: false,
            pushed_caps: None,
            eos_sent: false,
        }
    }

    fn set_caps(&mut self, caps: &gstreamer::Caps) {
        if self.caps.as_ref() == Some(caps) {
            return;
        }
        if let Some(structure) = caps.structure(0) {
            self.info.codec = structure.name().to_string();
            let dimension = |field| {
                structure
                    .get::<i32>(field)
                    .ok()
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(0)
            };
            self.info.width = dimension("width");
            self.info.height = dimension("height");
        }
        self.caps = Some(caps.clone());
    }

    fn remove(&mut self, key: i64) {
        if let Some(frame) = self.frames.remove(&key) {
            self.bytes = self.bytes.saturating_sub(frame.size());
            self.ranges = None;
        }
    }

    /// <https://w3c.github.io/media-source/#sourcebuffer-coded-frame-processing>
    fn insert(&mut self, key: i64, frame: Frame) {
        // A frame without a duration ends where the next one starts.
        if let Some(previous) = self
            .last_appended
            .and_then(|last| self.frames.get_mut(&last)) &&
            previous.guessed_end &&
            frame.pts > previous.pts
        {
            previous.end = frame.pts;
            previous.guessed_end = false;
            self.last_duration = previous.end - previous.pts;
            self.longest_duration = self.longest_duration.max(self.last_duration);
        }

        // Step 14. Remove the frames the new one overlaps.
        let duration = frame.end - frame.pts;
        let key_end = key.saturating_add(i64::try_from(duration).unwrap_or(i64::MAX).max(1));
        let overlapped: Vec<i64> = self.frames.range(key..key_end).map(|(k, _)| *k).collect();
        for overlapped in overlapped {
            self.remove(overlapped);
        }

        // Step 15. Remove what could only be decoded with a frame that is gone, or
        // that would now be decoded after a frame it does not follow: everything
        // from here to the next keyframe.
        let dependents: Vec<i64> = self
            .frames
            .range(key_end..)
            .take_while(|(_, frame)| !frame.keyframe)
            .map(|(k, _)| *k)
            .collect();
        for dependent in dependents {
            self.remove(dependent);
        }

        if !frame.guessed_end {
            self.last_duration = duration;
            self.longest_duration = self.longest_duration.max(duration);
        }
        self.bytes += frame.size();
        self.ranges = None;
        self.last_appended = Some(key);
        self.frames.insert(key, frame);
    }

    /// <https://w3c.github.io/media-source/#sourcebuffer-coded-frame-removal>
    fn remove_range(&mut self, range: &Range<u64>) {
        let mut removing_dependents = false;
        let mut removed = 0;
        self.frames.retain(|_, frame| {
            let remove = if range.contains(&frame.pts) {
                removing_dependents = true;
                true
            } else if frame.keyframe {
                removing_dependents = false;
                false
            } else {
                removing_dependents
            };
            if remove {
                removed += frame.size();
            }
            !remove
        });
        if removed > 0 {
            self.bytes = self.bytes.saturating_sub(removed);
            self.ranges = None;
        }
    }

    /// Drop the frames that playback left behind, a whole group of pictures at a
    /// time so that what stays can still be decoded.
    fn evict(&mut self, position: u64) {
        let Some(limit) = position.checked_sub(EVICTION_MARGIN) else {
            return;
        };
        let Some(first_kept) = self
            .frames
            .range(..=i64::try_from(limit).unwrap_or(i64::MAX))
            .rev()
            .find(|(_, frame)| frame.keyframe && frame.pts <= limit)
            .map(|(key, _)| *key)
        else {
            return;
        };
        let kept = self.frames.split_off(&first_kept);
        let evicted: usize = self.frames.values().map(Frame::size).sum();
        self.frames = kept;
        if evicted > 0 {
            self.bytes = self.bytes.saturating_sub(evicted);
            self.ranges = None;
        }
    }

    fn ranges(&mut self) -> &[Range<u64>] {
        let (frames, longest) = (&self.frames, self.longest_duration);
        self.ranges.get_or_insert_with(|| {
            let mut spans: Vec<(u64, u64)> = frames
                .values()
                .map(|frame| (frame.pts, frame.end))
                .collect();
            spans.sort_unstable();

            // Frames of one segment touch each other, give or take the rounding of
            // their timestamps; allow for more than that, as other engines do.
            let gap = longest.saturating_mul(2).clamp(1_000_000, MAX_GAP);
            let mut ranges: Vec<Range<u64>> = Vec::new();
            for (start, end) in spans {
                match ranges.last_mut() {
                    Some(last) if start <= last.end.saturating_add(gap) => {
                        last.end = last.end.max(end)
                    },
                    _ => ranges.push(start..end),
                }
            }
            ranges
        })
    }

    /// The decode timestamp of the frame to start with so that `position` gets
    /// presented, if the frames around `position` are here.
    fn start_for(&mut self, position: u64) -> Option<i64> {
        let range = self
            .ranges()
            .iter()
            .find(|range| range.start <= position.saturating_add(MAX_GAP) && position < range.end)?
            .clone();
        let position = position.max(range.start);

        // A frame is never decoded after it is presented, so the keyframe wanted
        // is among the frames with a decode timestamp up to `position`.
        self.frames
            .range(..=i64::try_from(position).unwrap_or(i64::MAX))
            .rev()
            .find(|(_, frame)| frame.keyframe && frame.pts <= position)
            .or_else(|| {
                self.frames
                    .iter()
                    .find(|(_, frame)| frame.keyframe && range.contains(&frame.pts))
            })
            .map(|(key, _)| *key)
    }

    /// Whether nothing at or after `position` is here.
    fn is_past_the_end(&mut self, position: u64) -> bool {
        self.ranges().last().is_none_or(|last| last.end <= position)
    }
}

struct Track {
    pad_name: String,
    buffer: Mutex<TrackBuffer>,
    /// The playback appsrc has asked for no more data.
    enough: AtomicBool,
    shared: Arc<Shared>,
}

impl Track {
    fn new(pad_name: &str, kind: SourceBufferTrackKind, shared: Arc<Shared>) -> Self {
        Self {
            pad_name: pad_name.to_owned(),
            buffer: Mutex::new(TrackBuffer::new(kind)),
            enough: AtomicBool::new(false),
            shared,
        }
    }

    /// Take a frame out of the demuxer.
    fn ingest(&self, sample: &gstreamer::Sample, timestamp_offset: i64, window: &Range<u64>) {
        let (Some(mut buffer), Some(caps)) = (sample.buffer_owned(), sample.caps_owned()) else {
            return;
        };
        let Some(pts) = buffer.pts() else {
            return;
        };

        // Move the timestamps from the segment of the demuxer to the media timeline.
        let segment_offset = sample
            .segment()
            .and_then(|segment| segment.downcast_ref::<gstreamer::ClockTime>())
            .and_then(|segment| Some((segment.time()?, segment.start()?)))
            .map_or(0, |(time, start)| {
                time.nseconds() as i64 - start.nseconds() as i64
            });
        let offset = segment_offset.saturating_add(timestamp_offset);
        let pts = (pts.nseconds() as i64).saturating_add(offset);
        let dts = buffer
            .dts()
            .map(|dts| (dts.nseconds() as i64).saturating_add(offset));
        let duration = buffer.duration().map(|duration| duration.nseconds());
        let keyframe = !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT);

        let mut track = lock(&self.buffer);
        track.set_caps(&caps);

        let duration_or_guess = duration.unwrap_or(track.last_duration);
        let end = pts.saturating_add(i64::try_from(duration_or_guess).unwrap_or(i64::MAX));

        // Steps 8 and 9 of coded frame processing: frames outside of the append
        // window are dropped, and so is what follows them up to a keyframe.
        let in_window = u64::try_from(pts).is_ok_and(|pts| pts >= window.start) &&
            u64::try_from(end).is_ok_and(|end| end <= window.end);
        if !in_window {
            track.need_keyframe = true;
            return;
        }
        if track.need_keyframe {
            if !keyframe {
                return;
            }
            track.need_keyframe = false;
        }
        let (pts, end) = (pts as u64, end as u64);

        {
            let buffer = buffer.make_mut();
            buffer.set_pts(gstreamer::ClockTime::from_nseconds(pts));
            buffer.set_dts(
                dts.and_then(|dts| u64::try_from(dts).ok())
                    .map(gstreamer::ClockTime::from_nseconds),
            );
        }

        track.insert(
            dts.unwrap_or(pts as i64),
            Frame {
                pts,
                end,
                keyframe,
                guessed_end: duration.is_none(),
                caps,
                buffer,
            },
        );
    }

    /// Push frames to the playback appsrc until it has enough or there are no
    /// more of them.
    fn pump(&self) {
        let mut track = lock(&self.buffer);
        let Some(appsrc) = track.appsrc.clone() else {
            return;
        };

        while !self.enough.load(Ordering::Relaxed) {
            let ended = self.shared.ended.load(Ordering::Relaxed);
            let next = match track.feed {
                Feed::From(position) => {
                    let next = track.start_for(position);
                    if next.is_none() && ended && track.is_past_the_end(position) {
                        track.send_end_of_stream(&appsrc);
                    }
                    next
                },
                Feed::After(last) => {
                    // Stop at a gap: what comes after it is not to be played until
                    // the gap is filled or playback seeks over it.
                    let limit = track.feed_end.saturating_add(MAX_GAP);
                    let next = track
                        .frames
                        .range((Excluded(last), Unbounded))
                        .next()
                        .filter(|(_, frame)| frame.pts <= limit)
                        .map(|(key, _)| *key);
                    if next.is_none() && ended {
                        track.send_end_of_stream(&appsrc);
                    }
                    next
                },
            };
            let Some(frame) = next.and_then(|key| track.frames.get(&key)) else {
                break;
            };
            let (buffer, caps, end) = (frame.buffer.clone(), frame.caps.clone(), frame.end);

            if track.pushed_caps.as_ref() != Some(&caps) {
                appsrc.set_caps(Some(&caps));
                track.pushed_caps = Some(caps);
            }
            if let Some(key) = next {
                track.feed = Feed::After(key);
            }
            track.feed_end = if track.pushed_any {
                track.feed_end.max(end)
            } else {
                end
            };
            track.pushed_any = true;

            // Fails while the appsrc is flushing, which a seek follows.
            if appsrc.push_buffer(buffer).is_err() {
                break;
            }
        }
    }

    fn seek(&self, position: u64) {
        let mut track = lock(&self.buffer);
        track.feed = Feed::From(position);
        track.feed_end = position;
        track.pushed_any = false;
        track.eos_sent = false;
    }

    /// The appsrc has been flushed and is to go on from `position`.
    fn seek_from_pipeline(&self, position: u64) {
        let pushed_any = {
            let mut track = lock(&self.buffer);
            // The flush undid the end of the stream too.
            track.eos_sent = false;
            track.pushed_any
        };
        // An appsrc that has been given nothing yet is where `MediaSource::seek`
        // left it, and is not to be taken back to the start by the seek to zero
        // every source does when it starts.
        if pushed_any {
            self.seek(position);
        }
    }
}

impl TrackBuffer {
    fn send_end_of_stream(&mut self, appsrc: &gstreamer_app::AppSrc) {
        if !self.eos_sent {
            self.eos_sent = true;
            // Fails while the appsrc is flushing, which a seek follows.
            if let Err(error) = appsrc.end_of_stream() {
                log::debug!("Could not end the stream of a track: {error:?}");
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Container {
    Mp4,
    WebM,
}

impl Container {
    fn from_media_type(media_type: &str) -> Option<Self> {
        let mime = media_type.parse::<Mime>().ok()?;
        if mime.type_() != mime::AUDIO && mime.type_() != mime::VIDEO {
            return None;
        }
        match mime.subtype().as_str() {
            "mp4" => Some(Container::Mp4),
            "webm" => Some(Container::WebM),
            _ => None,
        }
    }

    fn demuxer(&self) -> &'static str {
        match self {
            Container::Mp4 => "qtdemux",
            Container::WebM => "matroskademux",
        }
    }
}

enum AppendOutcome {
    Done,
    Failed(String),
}

/// What the frames coming out of the demuxer are adjusted with.
struct AppendSettings {
    /// Nanoseconds added to the timestamps.
    timestamp_offset: AtomicI64,
    /// Frames outside of this range of nanoseconds are dropped.
    window: Mutex<Range<u64>>,
}

struct SourceBuffer {
    container: Container,
    pipeline: gstreamer::Pipeline,
    appsrc: gstreamer_app::AppSrc,
    tracks: Arc<Mutex<Vec<Arc<Track>>>>,
    settings: Arc<AppendSettings>,
    outcome: Receiver<AppendOutcome>,
}

impl SourceBuffer {
    fn new(container: Container, shared: Arc<Shared>) -> Result<Self, PlayerError> {
        let tracks = Arc::new(Mutex::new(Vec::new()));
        let settings = Arc::new(AppendSettings {
            timestamp_offset: AtomicI64::new(0),
            window: Mutex::new(0..u64::MAX),
        });
        let (pipeline, appsrc, outcome) =
            Self::build_pipeline(container, shared, tracks.clone(), settings.clone())
                .map_err(|error| PlayerError::Backend(error.to_string()))?;
        Ok(Self {
            container,
            pipeline,
            appsrc,
            tracks,
            settings,
            outcome,
        })
    }

    fn build_pipeline(
        container: Container,
        shared: Arc<Shared>,
        tracks: Arc<Mutex<Vec<Arc<Track>>>>,
        settings: Arc<AppendSettings>,
    ) -> Result<
        (
            gstreamer::Pipeline,
            gstreamer_app::AppSrc,
            Receiver<AppendOutcome>,
        ),
        glib::BoolError,
    > {
        let pipeline = gstreamer::Pipeline::new();
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .build()?
            .downcast::<gstreamer_app::AppSrc>()
            .map_err(|_| glib::bool_error!("appsrc is not an AppSrc"))?;
        let demuxer = gstreamer::ElementFactory::make(container.demuxer()).build()?;
        pipeline.add_many([appsrc.upcast_ref(), &demuxer])?;
        appsrc.link(&demuxer)?;

        appsrc.set_format(gstreamer::Format::Bytes);
        appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);
        appsrc.set_max_bytes(0);

        let (sender, outcome) = mpsc::channel();

        // Everything from the appsrc to the appsinks runs in the streaming thread
        // of the appsrc, so once the marker gets here the frames of the bytes
        // pushed before it are all in their tracks.
        let marker_sender = Mutex::new(sender.clone());
        appsrc
            .static_pad("src")
            .ok_or_else(|| glib::bool_error!("appsrc has no src pad"))?
            .add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
                let is_marker = matches!(
                    info.data,
                    Some(gstreamer::PadProbeData::Buffer(ref buffer))
                        if buffer.size() == 0 && buffer.offset() == END_OF_APPEND_MARKER
                );
                if !is_marker {
                    return gstreamer::PadProbeReturn::Ok;
                }
                // Nobody is waiting if the source buffer is gone.
                let _ = lock(&marker_sender).send(AppendOutcome::Done);
                gstreamer::PadProbeReturn::Drop
            });

        let error_sender = Mutex::new(sender);
        pipeline
            .bus()
            .ok_or_else(|| glib::bool_error!("Pipeline has no bus"))?
            .set_sync_handler(move |_, message| {
                if let gstreamer::MessageView::Error(error) = message.view() {
                    let description = match error.debug() {
                        Some(debug) => format!("{} ({debug})", error.error()),
                        None => error.error().to_string(),
                    };
                    // Nobody is waiting if the source buffer is gone.
                    let _ = lock(&error_sender).send(AppendOutcome::Failed(description));
                }
                gstreamer::BusSyncReply::Drop
            });

        let appsinks: Mutex<HashMap<String, gstreamer::Element>> = Mutex::new(HashMap::new());
        demuxer.connect_pad_added(move |demuxer, pad| {
            let name = pad.name();
            let kind = if name.starts_with("video") {
                SourceBufferTrackKind::Video
            } else if name.starts_with("audio") {
                SourceBufferTrackKind::Audio
            } else {
                return;
            };

            // A later initialization segment brings the same tracks again.
            let track = {
                let mut tracks = lock(&tracks);
                match tracks.iter().find(|track| track.pad_name == name.as_str()) {
                    Some(track) => track.clone(),
                    None => {
                        let track = Arc::new(Track::new(name.as_str(), kind, shared.clone()));
                        tracks.push(track.clone());
                        track
                    },
                }
            };
            if let Some(caps) = pad.current_caps() {
                lock(&track.buffer).set_caps(&caps);
            }

            let link = || -> Result<gstreamer::Element, glib::BoolError> {
                let appsink = gstreamer::ElementFactory::make("appsink")
                    .property("sync", false)
                    .property("async", false)
                    .property("enable-last-sample", false)
                    .build()?
                    .downcast::<gstreamer_app::AppSink>()
                    .map_err(|_| glib::bool_error!("appsink is not an AppSink"))?;
                let settings = settings.clone();
                appsink.set_callbacks(
                    gstreamer_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = appsink
                                .pull_sample()
                                .map_err(|_| gstreamer::FlowError::Eos)?;
                            let window = lock(&settings.window).clone();
                            track.ingest(
                                &sample,
                                settings.timestamp_offset.load(Ordering::Relaxed),
                                &window,
                            );
                            Ok(gstreamer::FlowSuccess::Ok)
                        })
                        .build(),
                );

                let bin = demuxer
                    .parent()
                    .and_then(|parent| parent.downcast::<gstreamer::Bin>().ok())
                    .ok_or_else(|| glib::bool_error!("Demuxer is not in a pipeline"))?;
                bin.add(&appsink)?;
                let sink_pad = appsink
                    .static_pad("sink")
                    .ok_or_else(|| glib::bool_error!("appsink has no sink pad"))?;
                pad.link(&sink_pad)
                    .map_err(|error| glib::bool_error!("Could not link appsink: {error}"))?;
                appsink.sync_state_with_parent()?;
                Ok(appsink.upcast())
            };

            match link() {
                Ok(appsink) => {
                    // The pad this track had before is gone, and so is the use of
                    // its appsink.
                    if let Some(old) = lock(&appsinks).insert(name.to_string(), appsink) {
                        if let Err(error) = old.set_state(gstreamer::State::Null) {
                            log::warn!("Could not stop the appsink of a replaced pad: {error}");
                        }
                        if let Some(bin) = old
                            .parent()
                            .and_then(|parent| parent.downcast::<gstreamer::Bin>().ok()) &&
                            let Err(error) = bin.remove(&old)
                        {
                            log::warn!("Could not remove the appsink of a replaced pad: {error}");
                        }
                    }
                },
                Err(error) => log::error!("Could not take the frames of {name}: {error}"),
            }
        });

        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|error| glib::bool_error!("Could not start append pipeline: {error}"))?;

        Ok((pipeline, appsrc, outcome))
    }

    fn tracks(&self) -> Vec<Arc<Track>> {
        lock(&self.tracks).clone()
    }

    /// Make room for an append, as the coded frame eviction algorithm asks.
    /// <https://w3c.github.io/media-source/#sourcebuffer-coded-frame-eviction>
    fn make_room(&self, position: u64) -> Result<(), PlayerError> {
        for track in self.tracks() {
            let mut track = lock(&track.buffer);
            let quota = match track.info.kind {
                SourceBufferTrackKind::Video => VIDEO_QUOTA,
                SourceBufferTrackKind::Audio => AUDIO_QUOTA,
            };
            if track.bytes > quota {
                track.evict(position);
            }
            if track.bytes > quota {
                return Err(PlayerError::QuotaExceeded);
            }
        }
        Ok(())
    }

    fn append(&self, data: Vec<u8>) -> Result<(), PlayerError> {
        // Whatever an earlier append left behind is not about this one.
        while self.outcome.try_recv().is_ok() {}

        let failed = |error: gstreamer::FlowError| PlayerError::AppendFailed(error.to_string());
        self.appsrc
            .push_buffer(gstreamer::Buffer::from_slice(data))
            .map_err(failed)?;

        let mut marker = gstreamer::Buffer::new();
        if let Some(marker) = marker.get_mut() {
            marker.set_offset(END_OF_APPEND_MARKER);
        }
        self.appsrc.push_buffer(marker).map_err(failed)?;

        match self.outcome.recv_timeout(APPEND_TIMEOUT) {
            Ok(AppendOutcome::Done) => Ok(()),
            Ok(AppendOutcome::Failed(error)) => Err(PlayerError::AppendFailed(error)),
            Err(_) => Err(PlayerError::AppendFailed(
                "The demuxer did not finish in time".to_owned(),
            )),
        }
    }

    /// <https://w3c.github.io/media-source/#sourcebuffer-reset-parser-state>
    fn reset_parser(&self) {
        // The demuxer forgets the segment it was in the middle of and keeps the
        // tracks of the initialization segment.
        if !self.appsrc.send_event(gstreamer::event::FlushStart::new()) ||
            !self
                .appsrc
                .send_event(gstreamer::event::FlushStop::new(true))
        {
            log::warn!("Could not flush the append pipeline");
        }
        for track in self.tracks() {
            let mut track = lock(&track.buffer);
            track.last_appended = None;
            track.need_keyframe = true;
        }
    }

    fn buffered(&self) -> Vec<Range<u64>> {
        let mut buffered: Option<Vec<Range<u64>>> = None;
        for track in self.tracks() {
            let mut track = lock(&track.buffer);
            let ranges = track.ranges();
            buffered = Some(match buffered {
                Some(buffered) => intersect(&buffered, ranges),
                None => ranges.to_vec(),
            });
        }
        buffered.unwrap_or_default()
    }

    fn state(&self) -> SourceBufferState {
        SourceBufferState {
            tracks: self
                .tracks()
                .iter()
                .map(|track| lock(&track.buffer).info.clone())
                .collect(),
            buffered: self
                .buffered()
                .into_iter()
                .map(|range| seconds(range.start)..seconds(range.end))
                .collect(),
        }
    }
}

impl Drop for SourceBuffer {
    fn drop(&mut self) {
        if let Err(error) = self.pipeline.set_state(gstreamer::State::Null) {
            log::warn!("Could not stop append pipeline: {error}");
        }
    }
}

/// The source buffers of a player and what plays them.
#[derive(Default)]
pub(crate) struct MediaSource {
    shared: Arc<Shared>,
    source_buffers: Vec<(SourceBufferId, SourceBuffer)>,
    next_id: u32,
    /// Nanoseconds.
    duration: Option<u64>,
    /// The tracks being played, once playback has started.
    playing: Vec<Arc<Track>>,
}

impl MediaSource {
    fn source_buffer(&self, id: SourceBufferId) -> Result<&SourceBuffer, PlayerError> {
        self.source_buffers
            .iter()
            .find(|(source_buffer_id, _)| *source_buffer_id == id)
            .map(|(_, source_buffer)| source_buffer)
            .ok_or(PlayerError::UnknownSourceBuffer)
    }

    fn pump(&self) {
        for track in &self.playing {
            track.pump();
        }
    }

    pub(crate) fn add_source_buffer(
        &mut self,
        media_type: &str,
    ) -> Result<SourceBufferId, PlayerError> {
        let container =
            Container::from_media_type(media_type).ok_or(PlayerError::UnsupportedMediaType)?;
        let source_buffer = SourceBuffer::new(container, self.shared.clone())?;
        let id = SourceBufferId(self.next_id);
        self.next_id += 1;
        self.source_buffers.push((id, source_buffer));
        Ok(id)
    }

    pub(crate) fn remove_source_buffer(&mut self, id: SourceBufferId) -> Result<(), PlayerError> {
        self.source_buffer(id)?;
        self.source_buffers
            .retain(|(source_buffer_id, _)| *source_buffer_id != id);
        Ok(())
    }

    /// The source buffer goes on with data of `media_type`, which its demuxer has
    /// to be able to take.
    pub(crate) fn change_type(
        &self,
        id: SourceBufferId,
        media_type: &str,
    ) -> Result<(), PlayerError> {
        let source_buffer = self.source_buffer(id)?;
        if Container::from_media_type(media_type) != Some(source_buffer.container) {
            return Err(PlayerError::UnsupportedMediaType);
        }
        source_buffer.reset_parser();
        Ok(())
    }

    pub(crate) fn append(
        &self,
        id: SourceBufferId,
        data: Vec<u8>,
    ) -> Result<SourceBufferState, PlayerError> {
        let source_buffer = self.source_buffer(id)?;
        source_buffer.make_room(self.shared.position.load(Ordering::Relaxed))?;
        source_buffer.append(data)?;
        // These frames may be the ones playback was waiting for.
        self.pump();
        Ok(source_buffer.state())
    }

    pub(crate) fn reset_parser(&self, id: SourceBufferId) -> Result<(), PlayerError> {
        self.source_buffer(id)?.reset_parser();
        Ok(())
    }

    pub(crate) fn remove_range(
        &self,
        id: SourceBufferId,
        range: Range<f64>,
    ) -> Result<SourceBufferState, PlayerError> {
        let source_buffer = self.source_buffer(id)?;
        let range = nanoseconds(range.start)..nanoseconds(range.end);
        for track in source_buffer.tracks() {
            lock(&track.buffer).remove_range(&range);
        }
        Ok(source_buffer.state())
    }

    pub(crate) fn set_timestamp_offset(
        &self,
        id: SourceBufferId,
        offset: f64,
    ) -> Result<(), PlayerError> {
        let offset = (offset * NANOSECONDS).round() as i64;
        self.source_buffer(id)?
            .settings
            .timestamp_offset
            .store(offset, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn set_append_window(
        &self,
        id: SourceBufferId,
        window: Range<f64>,
    ) -> Result<(), PlayerError> {
        *lock(&self.source_buffer(id)?.settings.window) =
            nanoseconds(window.start)..nanoseconds(window.end);
        Ok(())
    }

    /// The ranges in seconds that every source buffer has.
    pub(crate) fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered: Option<Vec<Range<u64>>> = None;
        for (_, source_buffer) in &self.source_buffers {
            let ranges = source_buffer.buffered();
            buffered = Some(match buffered {
                Some(buffered) => intersect(&buffered, &ranges),
                None => ranges,
            });
        }
        buffered
            .unwrap_or_default()
            .into_iter()
            .map(|range| seconds(range.start)..seconds(range.end))
            .collect()
    }

    pub(crate) fn duration(&self) -> Option<f64> {
        self.duration.map(seconds)
    }

    pub(crate) fn set_duration(&mut self, duration: Option<f64>) {
        self.duration = duration.map(nanoseconds);
        for track in &self.playing {
            if let Some(appsrc) = lock(&track.buffer).appsrc.as_ref() {
                set_appsrc_duration(appsrc, self.duration);
            }
        }
    }

    pub(crate) fn set_ended(&self, ended: bool) {
        self.shared.ended.store(ended, Ordering::Relaxed);
        self.pump();
    }

    pub(crate) fn set_position(&self, position: f64) {
        self.shared
            .position
            .store(nanoseconds(position), Ordering::Relaxed);
    }

    /// Playback is about to move to `position`. The pipeline tells the tracks so
    /// itself when it seeks, but it cannot seek before it has shown a first
    /// frame, and it never will if the frames it starts with are not the ones
    /// that are going to be appended.
    pub(crate) fn seek(&self, position: f64) {
        self.set_position(position);
        for track in &self.playing {
            let pushed_any = lock(&track.buffer).pushed_any;
            if !pushed_any {
                track.seek(nanoseconds(position));
            }
        }
        self.pump();
    }

    /// Give the source element of the playback pipeline an appsrc for every
    /// track.
    pub(crate) fn start(&mut self, source: &ServoMediaSourceSrc) -> Result<(), glib::BoolError> {
        let start = self.shared.position.load(Ordering::Relaxed);
        for (_, source_buffer) in &self.source_buffers {
            for track in source_buffer.tracks() {
                let appsrc = gstreamer::ElementFactory::make("appsrc")
                    .build()?
                    .downcast::<gstreamer_app::AppSrc>()
                    .map_err(|_| glib::bool_error!("appsrc is not an AppSrc"))?;
                appsrc.set_format(gstreamer::Format::Time);
                appsrc.set_stream_type(gstreamer_app::AppStreamType::Seekable);
                appsrc.set_max_bytes(PLAYBACK_QUEUE_BYTES);
                if appsrc.find_property("max-time").is_some() {
                    appsrc.set_property("max-time", PLAYBACK_QUEUE_TIME);
                }
                set_appsrc_duration(&appsrc, self.duration);

                let (need_data, enough_data, seek_data) =
                    (track.clone(), track.clone(), track.clone());
                appsrc.set_callbacks(
                    gstreamer_app::AppSrcCallbacks::builder()
                        .need_data(move |_, _| {
                            need_data.enough.store(false, Ordering::Relaxed);
                            need_data.pump();
                        })
                        .enough_data(move |_| {
                            enough_data.enough.store(true, Ordering::Relaxed);
                        })
                        .seek_data(move |_, position| {
                            seek_data.seek_from_pipeline(position);
                            true
                        })
                        .build(),
                );

                {
                    let mut buffer = lock(&track.buffer);
                    appsrc.set_caps(buffer.caps.as_ref());
                    buffer.pushed_caps = buffer.caps.clone();
                    buffer.appsrc = Some(appsrc.clone());
                }
                track.seek(start);
                source.add_track(&appsrc)?;
                self.playing.push(track);
            }
        }
        Ok(())
    }

    /// Let go of the playback pipeline.
    pub(crate) fn stop(&mut self) {
        for track in self.playing.drain(..) {
            lock(&track.buffer).appsrc = None;
        }
    }
}

fn set_appsrc_duration(appsrc: &gstreamer_app::AppSrc, duration: Option<u64>) {
    // The appsrc takes the largest value as an unknown duration.
    appsrc.set_property("duration", duration.unwrap_or(u64::MAX));
}
