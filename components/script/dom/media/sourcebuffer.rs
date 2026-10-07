/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::cformat;
use script_bindings::reflector::reflect_dom_object;
use servo_media::player::{
    PlayerError, SourceBufferId, SourceBufferState, SourceBufferTrackKind,
};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceReadyState,
};
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// <https://w3c.github.io/media-source/#sourcebuffer>
#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    /// The media source that created this source buffer. None once the source
    /// buffer has been removed from it.
    media_source: MutNullableDom<MediaSource>,
    /// Identifies the source buffer in the media player.
    id: u32,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    mode: Cell<AppendMode>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    updating: Cell<bool>,
    /// Incremented whenever the tasks that end an update are cancelled.
    update_generation: Cell<u32>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    timestamp_offset: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    append_window_start: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    append_window_end: Cell<f64>,
    /// What the media player held after the last append or removal.
    buffered: DomRefCell<TimeRangesContainer>,
    /// <https://w3c.github.io/media-source/#dfn-first-initialization-segment-received-flag>
    has_initialization_segment: Cell<bool>,
    has_audio: Cell<bool>,
    has_video: Cell<bool>,
    video_width: Cell<u32>,
    video_height: Cell<u32>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource, id: SourceBufferId) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            media_source: MutNullableDom::new(Some(media_source)),
            id: id.0,
            mode: Cell::new(AppendMode::Segments),
            updating: Cell::new(false),
            update_generation: Cell::new(0),
            timestamp_offset: Cell::new(0.),
            append_window_start: Cell::new(0.),
            append_window_end: Cell::new(f64::INFINITY),
            buffered: Default::default(),
            has_initialization_segment: Cell::new(false),
            has_audio: Cell::new(false),
            has_video: Cell::new(false),
            video_width: Cell::new(0),
            video_height: Cell::new(0),
        }
    }

    pub(crate) fn new(
        window: &Window,
        media_source: &MediaSource,
        id: SourceBufferId,
        can_gc: CanGc,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            Box::new(SourceBuffer::new_inherited(media_source, id)),
            window,
            can_gc,
        )
    }

    pub(crate) fn id(&self) -> SourceBufferId {
        SourceBufferId(self.id)
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }

    pub(crate) fn has_initialization_segment(&self) -> bool {
        self.has_initialization_segment.get()
    }

    pub(crate) fn has_audio(&self) -> bool {
        self.has_audio.get()
    }

    /// The size of the video track, if the source buffer has got one.
    pub(crate) fn video_size(&self) -> Option<(u32, u32)> {
        self.has_video
            .get()
            .then(|| (self.video_width.get(), self.video_height.get()))
    }

    /// The end of the last buffered range, or zero if nothing is buffered.
    pub(crate) fn highest_buffered_end(&self) -> f64 {
        let buffered = self.buffered.borrow();
        match buffered.len() {
            0 => 0.,
            length => buffered.end(length - 1).unwrap_or(0.),
        }
    }

    /// The media source has let go of this source buffer.
    pub(crate) fn removed_from_media_source(&self) {
        if self.updating.get() {
            self.abort_update();
        }
        self.media_source.set(None);
    }

    /// The parent media source, as long as this source buffer has not been
    /// removed from it and is not being updated.
    fn media_source_for_change(&self) -> Fallible<DomRoot<MediaSource>> {
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        Ok(media_source)
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_event: move |cx| {
                let this = this.root();
                this.upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }

    fn begin_update(&self) {
        self.updating.set(true);
        self.queue_event("updatestart");
    }

    /// Queue the task that ends the update in progress. A failed update is an
    /// append whose bytes could not be parsed.
    /// <https://w3c.github.io/media-source/#sourcebuffer-append-error>
    fn queue_end_of_update(&self, failed: bool) {
        let this = Trusted::new(self);
        let generation = self.update_generation.get();
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_end_of_update: move |cx| {
                let this = this.root();
                if generation != this.update_generation.get() {
                    return;
                }

                this.updating.set(false);
                let target = this.upcast::<EventTarget>();
                if failed {
                    target.fire_event(cx, Atom::from("error"));
                } else {
                    target.fire_event(cx, Atom::from("update"));
                }
                target.fire_event(cx, Atom::from("updateend"));

                if failed && let Some(media_source) = this.media_source.get() {
                    media_source.end_of_stream(Some(EndOfStreamError::Decode), cx);
                }
            }));
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn abort_update(&self) {
        self.update_generation
            .set(self.update_generation.get().wrapping_add(1));
        self.updating.set(false);
        self.queue_event("abort");
        self.queue_event("updateend");
    }

    fn set_state(&self, state: &SourceBufferState) {
        let mut buffered = TimeRangesContainer::default();
        for range in &state.buffered {
            if let Err(error) = buffered.add(range.start, range.end) {
                warn!("Ignoring a buffered range of the media player: {error:?}");
            }
        }
        *self.buffered.borrow_mut() = buffered;

        if state.tracks.is_empty() {
            return;
        }
        self.has_initialization_segment.set(true);
        self.has_audio.set(false);
        self.has_video.set(false);
        for track in &state.tracks {
            match track.kind {
                SourceBufferTrackKind::Audio => self.has_audio.set(true),
                SourceBufferTrackKind::Video => {
                    self.has_video.set(true);
                    self.video_width.set(track.width);
                    self.video_height.set(track.height);
                },
            }
        }
    }

    fn set_append_window(&self, media_source: &MediaSource) {
        let window = self.append_window_start.get()..self.append_window_end.get();
        if let Err(error) = media_source
            .with_player(|player| player.set_source_buffer_append_window(self.id(), window))
        {
            warn!("Could not set the append window of a source buffer: {error:?}");
        }
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        self.mode.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, mode: AppendMode) -> ErrorResult {
        // Step 1. If this object has been removed from the sourceBuffers attribute of the
        // parent media source, then throw an InvalidStateError exception.
        // Step 2. If the updating attribute equals true, then throw an InvalidStateError
        // exception.
        let media_source = self.media_source_for_change()?;

        // Step 5. If the readyState attribute of the parent media source is in the "ended"
        // state, then set it to "open" and fire an event named sourceopen.
        media_source.reopen_if_ended();

        if mode == AppendMode::Sequence {
            // FIXME: The media player places the coded frames by their own timestamps
            // whatever the mode is.
            warn!("SourceBuffer.mode \"sequence\" is not implemented, timestamps are kept");
        }

        // Step 8. Update the attribute to the new value.
        self.mode.set(mode);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, can_gc: CanGc) -> Fallible<DomRoot<TimeRanges>> {
        // Step 1. If this object has been removed from the sourceBuffers attribute of the
        // parent media source then throw an InvalidStateError exception.
        if self.media_source.get().is_none() {
            return Err(Error::InvalidState(None));
        }

        Ok(TimeRanges::new(
            self.global().as_window(),
            self.buffered.borrow().clone(),
            can_gc,
        ))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.timestamp_offset.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, offset: Finite<f64>) -> ErrorResult {
        // Steps 2-3.
        let media_source = self.media_source_for_change()?;

        // Step 4. If the readyState attribute of the parent media source is in the "ended"
        // state, then set it to "open" and fire an event named sourceopen.
        media_source.reopen_if_ended();

        // Step 7. Update the attribute to the new value.
        media_source
            .with_player(|player| player.set_source_buffer_timestamp_offset(self.id(), *offset))
            .map_err(|_| Error::InvalidState(None))?;
        self.timestamp_offset.set(*offset);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, start: Finite<f64>) -> ErrorResult {
        // Steps 1-2.
        let media_source = self.media_source_for_change()?;

        // Step 3. If the new value is less than 0 or greater than or equal to
        // appendWindowEnd then throw a TypeError exception.
        if *start < 0. || *start >= self.append_window_end.get() {
            return Err(Error::Type(cformat!(
                "appendWindowStart is out of range: {}",
                *start
            )));
        }

        // Step 4. Update the attribute to the new value.
        self.append_window_start.set(*start);
        self.set_append_window(&media_source);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, end: f64) -> ErrorResult {
        // Steps 1-2.
        let media_source = self.media_source_for_change()?;

        // Step 3. If the new value equals NaN, then throw a TypeError.
        // Step 4. If the new value is less than or equal to appendWindowStart then throw a
        // TypeError exception.
        if end.is_nan() || end <= self.append_window_start.get() {
            return Err(Error::Type(cformat!(
                "appendWindowEnd is out of range: {}",
                end
            )));
        }

        // Step 5. Update the attribute to the new value.
        self.append_window_end.set(end);
        self.set_append_window(&media_source);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdatestart
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdate
    event_handler!(update, GetOnupdate, SetOnupdate);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdateend
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onerror
    event_handler!(error, GetOnerror, SetOnerror);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, cx: &mut JSContext, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        // Step 1. Run the prepare append algorithm.
        // <https://w3c.github.io/media-source/#sourcebuffer-prepare-append>
        let media_source = self.media_source_for_change()?;
        if media_source.media_element_has_error() {
            return Err(Error::InvalidState(None));
        }
        media_source.reopen_if_ended();

        // Step 2. Add data to the end of the input buffer.
        let data = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };

        // The media player parses the bytes before it returns, which is what lets it
        // tell here that the source buffer is full. The steps of the buffer append
        // algorithm that follow only have the events left to fire.
        let appended = if data.is_empty() {
            Ok(None)
        } else {
            media_source
                .with_player(|player| player.append_data(self.id(), data))
                .map(Some)
        };
        let failed = match appended {
            Ok(Some(state)) => {
                self.set_state(&state);
                false
            },
            Ok(None) => false,
            Err(PlayerError::QuotaExceeded) => {
                return Err(Error::QuotaExceeded {
                    quota: None,
                    requested: None,
                });
            },
            Err(error) => {
                warn!("Could not append to a source buffer: {error:?}");
                true
            },
        };

        // Step 3. Set the updating attribute to true.
        // Step 4. Queue a task to fire an event named updatestart at this SourceBuffer
        // object.
        self.begin_update();

        // Step 5. Asynchronously run the buffer append algorithm.
        self.queue_end_of_update(failed);

        if !failed {
            media_source.source_buffer_changed(cx);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        // Step 1. If this object has been removed from the sourceBuffers attribute of the
        // parent media source then throw an InvalidStateError exception.
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };

        // Step 2. If the readyState attribute of the parent media source is not in the
        // "open" state then throw an InvalidStateError exception.
        if media_source.ready_state() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 3 does not apply, a range removal is over before remove() returns.

        // Step 4. If the updating attribute equals true, then abort the buffer append
        // algorithm, set the updating attribute to false, and fire the abort and
        // updateend events.
        if self.updating.get() {
            self.abort_update();
        }

        // Step 5. Run the reset parser state algorithm.
        if let Err(error) =
            media_source.with_player(|player| player.reset_source_buffer_parser(self.id()))
        {
            warn!("Could not reset the parser of a source buffer: {error:?}");
        }

        // Step 6. Set appendWindowStart to the presentation start time.
        self.append_window_start.set(0.);

        // Step 7. Set appendWindowEnd to positive Infinity.
        self.append_window_end.set(f64::INFINITY);
        self.set_append_window(&media_source);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(cformat!("The media type is empty")));
        }

        // Steps 2-3.
        let media_source = self.media_source_for_change()?;

        // Step 4. If type contains a MIME type that is not supported, then throw a
        // NotSupportedError exception.
        let media_type = String::from(type_);
        if !MediaSource::is_type_supported(&media_type) {
            return Err(Error::NotSupported(None));
        }

        // Step 5. If the readyState attribute of the parent media source is in the "ended"
        // state, then set it to "open" and fire an event named sourceopen.
        media_source.reopen_if_ended();

        // Step 6. Run the reset parser state algorithm.
        // The media player does it.
        media_source
            .with_player(|player| player.change_source_buffer_type(self.id(), &media_type))
            .map_err(|_| Error::NotSupported(None))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, cx: &mut JSContext, start: Finite<f64>, end: f64) -> ErrorResult {
        // Steps 1-2.
        let media_source = self.media_source_for_change()?;

        // Step 3. If duration equals NaN, then throw a TypeError exception.
        let duration = media_source.duration();
        if duration.is_nan() {
            return Err(Error::Type(cformat!("The media source has no duration")));
        }

        // Step 4. If start is negative or greater than duration, then throw a TypeError
        // exception.
        if *start < 0. || *start > duration {
            return Err(Error::Type(cformat!("start is out of range: {}", *start)));
        }

        // Step 5. If end is less than or equal to start or end equals NaN, then throw a
        // TypeError exception.
        if end.is_nan() || end <= *start {
            return Err(Error::Type(cformat!("end is out of range: {}", end)));
        }

        // Step 6. If the readyState attribute of the parent media source is in the "ended"
        // state, then set it to "open" and fire an event named sourceopen.
        media_source.reopen_if_ended();

        // Step 7. Run the range removal algorithm.
        // <https://w3c.github.io/media-source/#sourcebuffer-range-removal>
        let state = media_source
            .with_player(|player| player.remove_source_buffer_range(self.id(), *start..end))
            .map_err(|_| Error::InvalidState(None))?;
        self.set_state(&state);
        self.begin_update();
        self.queue_end_of_update(false);
        media_source.source_buffer_changed(cx);
        Ok(())
    }
}
