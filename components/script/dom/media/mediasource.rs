/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use mime::Mime;
use script_bindings::cformat;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::player::{Player, PlayerError};
use servo_media::{ServoMedia, SupportsMediaType};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaErrorBinding::MediaErrorConstants::{
    MEDIA_ERR_DECODE, MEDIA_ERR_NETWORK,
};
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, MediaSourceReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// <https://w3c.github.io/media-source/#mediasource>
#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    ready_state: Cell<MediaSourceReadyState>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    duration: Cell<f64>,
    /// The media element this media source is attached to, if any.
    media_element: MutNullableDom<HTMLMediaElement>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    source_buffers: Dom<SourceBufferList>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    active_source_buffers: Dom<SourceBufferList>,
    /// Whether the media player has been given the tracks of the source buffers.
    /// Source buffers cannot be added from then on.
    started: Cell<bool>,
    /// <https://w3c.github.io/media-source/#dfn-live-seekable-range>
    live_seekable_range: Cell<Option<LiveSeekableRange>>,
}

/// <https://w3c.github.io/media-source/#dfn-live-seekable-range>
#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
struct LiveSeekableRange {
    start: f64,
    end: f64,
}

impl MediaSource {
    fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            ready_state: Cell::new(MediaSourceReadyState::Closed),
            duration: Cell::new(f64::NAN),
            media_element: Default::default(),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            started: Cell::new(false),
            live_seekable_range: Cell::new(None),
        }
    }

    fn new_with_proto(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
    ) -> DomRoot<MediaSource> {
        let source_buffers = SourceBufferList::new(window, can_gc);
        let active_source_buffers = SourceBufferList::new(window, can_gc);
        reflect_dom_object_with_proto(
            Box::new(MediaSource::new_inherited(
                &source_buffers,
                &active_source_buffers,
            )),
            window,
            proto,
            can_gc,
        )
    }

    pub(crate) fn ready_state(&self) -> MediaSourceReadyState {
        self.ready_state.get()
    }

    /// The duration, which is not a number while the media source is closed.
    pub(crate) fn duration(&self) -> f64 {
        self.duration.get()
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ready_state.get() == MediaSourceReadyState::Ended
    }

    pub(crate) fn live_seekable_range(&self) -> Option<(f64, f64)> {
        self.live_seekable_range
            .get()
            .map(|range| (range.start, range.end))
    }

    pub(crate) fn media_element_has_error(&self) -> bool {
        self.media_element
            .get()
            .is_some_and(|element| element.in_error_state())
    }

    /// Whether source buffers can be created for the given media type.
    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    pub(crate) fn is_type_supported(media_type: &str) -> bool {
        let Ok(mime) = media_type.parse::<Mime>() else {
            return false;
        };
        // The containers the media player can take coded frames out of.
        let is_supported_container = matches!(
            (mime.type_().as_str(), mime.subtype().as_str()),
            ("audio" | "video", "mp4" | "webm")
        );
        is_supported_container &&
            ServoMedia::get().can_play_type(media_type) != SupportsMediaType::No
    }

    /// Run the given closure with the media player of the media element this
    /// media source is attached to.
    pub(crate) fn with_player<R>(
        &self,
        f: impl FnOnce(&dyn Player) -> Result<R, PlayerError>,
    ) -> Result<R, PlayerError> {
        let player = self
            .media_element
            .get()
            .and_then(|element| element.media_player())
            .ok_or(PlayerError::MediaSourceUnsupported)?;
        let player = player
            .lock()
            .map_err(|_| PlayerError::Backend("The media player lock is poisoned".to_owned()))?;
        f(&*player)
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(media_source_event: move |cx| {
                let this = this.root();
                this.upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }

    /// Returns false if the media source is attached already.
    /// <https://w3c.github.io/media-source/#mediasource-attach>
    pub(crate) fn attach(&self, element: &HTMLMediaElement) -> bool {
        // If readyState is NOT set to "closed": Run the "If the media data cannot be
        // fetched at all" steps of the resource fetch algorithm.
        if self.ready_state.get() != MediaSourceReadyState::Closed {
            return false;
        }

        // Step 2. Set the readyState attribute to "open".
        // Step 3. Queue a task to fire an event named sourceopen at the MediaSource.
        self.media_element.set(Some(element));
        self.started.set(false);
        self.ready_state.set(MediaSourceReadyState::Open);
        self.queue_event("sourceopen");
        true
    }

    /// <https://w3c.github.io/media-source/#mediasource-detach>
    pub(crate) fn detach(&self) {
        self.media_element.set(None);
        self.started.set(false);
        self.live_seekable_range.set(None);

        // Step 3. Set the readyState attribute to "closed".
        self.ready_state.set(MediaSourceReadyState::Closed);

        // Step 4. Update duration to NaN.
        self.duration.set(f64::NAN);

        // Step 5. Remove all the SourceBuffer objects from activeSourceBuffers.
        // Step 6. Queue a task to fire an event named removesourcebuffer at
        // activeSourceBuffers.
        self.active_source_buffers.clear();

        // Step 7. Remove all the SourceBuffer objects from sourceBuffers.
        // Step 8. Queue a task to fire an event named removesourcebuffer at sourceBuffers.
        for source_buffer in self.source_buffers.to_vec() {
            source_buffer.removed_from_media_source();
        }
        self.source_buffers.clear();

        // Step 9. Queue a task to fire an event named sourceclose at the MediaSource.
        self.queue_event("sourceclose");
    }

    /// If the readyState attribute is in the "ended" state then set it to "open" and
    /// queue a task to fire an event named sourceopen.
    pub(crate) fn reopen_if_ended(&self) {
        if !self.is_ended() {
            return;
        }
        self.ready_state.set(MediaSourceReadyState::Open);
        self.queue_event("sourceopen");
        if let Err(error) = self.with_player(|player| player.set_media_source_ended(false)) {
            warn!("Could not reopen the media source of the media player: {error:?}");
        }
    }

    fn is_any_source_buffer_updating(&self) -> bool {
        self.source_buffers
            .to_vec()
            .iter()
            .any(|source_buffer| source_buffer.updating())
    }

    /// The largest end time of the buffered ranges of the source buffers.
    fn highest_buffered_end(&self) -> f64 {
        self.source_buffers
            .to_vec()
            .iter()
            .map(|source_buffer| source_buffer.highest_buffered_end())
            .fold(0., f64::max)
    }

    /// <https://w3c.github.io/media-source/#duration-change-algorithm>
    fn change_duration(&self, new_duration: f64) -> ErrorResult {
        // Step 1. If the current value of duration is equal to new duration, then return.
        if self.duration.get() == new_duration {
            return Ok(());
        }

        // Step 2. If new duration is less than the highest presentation timestamp of any
        // buffered coded frames for all SourceBuffer objects in sourceBuffers, then throw
        // an InvalidStateError exception.
        // The end of the last buffered frame is what is known here, which makes this
        // stricter than the specification by the duration of that frame.
        if new_duration < self.highest_buffered_end() {
            return Err(Error::InvalidState(None));
        }

        // Step 5. Update duration to new duration.
        self.duration.set(new_duration);

        // Step 6. Update the media element's duration to new duration.
        let player_duration = new_duration.is_finite().then_some(new_duration);
        if let Err(error) =
            self.with_player(|player| player.set_media_source_duration(player_duration))
        {
            warn!("Could not set the duration of the media player: {error:?}");
        }
        if let Some(element) = self.media_element.get() {
            element.media_source_duration_changed(new_duration);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#end-of-stream-algorithm>
    pub(crate) fn end_of_stream(&self, error: Option<EndOfStreamError>, cx: &mut JSContext) {
        // Step 1. Change the readyState attribute value to "ended".
        // Step 2. Queue a task to fire an event named sourceended at the MediaSource.
        self.ready_state.set(MediaSourceReadyState::Ended);
        self.queue_event("sourceended");

        let Some(element) = self.media_element.get() else {
            return;
        };

        // Step 3.
        match error {
            None => {
                // Run the duration change algorithm with new duration set to the largest
                // track buffer ranges end time across all the track buffers across all
                // SourceBuffer objects in sourceBuffers.
                if let Err(error) = self.change_duration(self.highest_buffered_end()) {
                    warn!("Could not set the duration of an ended media source: {error:?}");
                }

                // Notify the media element that it now has all of the media data.
                if let Err(error) = self.with_player(|player| player.set_media_source_ended(true))
                {
                    warn!("Could not end the media source of the media player: {error:?}");
                }
                element.media_source_data_changed();
            },
            Some(EndOfStreamError::Network) => element.media_source_failed(MEDIA_ERR_NETWORK, cx),
            Some(EndOfStreamError::Decode) => element.media_source_failed(MEDIA_ERR_DECODE, cx),
        }
    }

    /// A source buffer holds something else than it did.
    pub(crate) fn source_buffer_changed(&self, cx: &mut JSContext) {
        let Some(element) = self.media_element.get() else {
            return;
        };

        let source_buffers = self.source_buffers.to_vec();
        for source_buffer in &source_buffers {
            if source_buffer.has_initialization_segment() {
                self.active_source_buffers.add(source_buffer);
            }
        }

        // <https://w3c.github.io/media-source/#sourcebuffer-init-segment-received>
        // The media element gets its metadata once the first initialization segment
        // received flag of every source buffer is true.
        let has_all_initialization_segments = !source_buffers.is_empty() &&
            source_buffers
                .iter()
                .all(|source_buffer| source_buffer.has_initialization_segment());
        if !self.started.get() && has_all_initialization_segments {
            // If the duration attribute is NaN, run the duration change algorithm. The
            // media player does not tell the duration found in an initialization
            // segment, so it is unbounded until the page sets it.
            if self.duration.get().is_nan() &&
                let Err(error) = self.change_duration(f64::INFINITY)
            {
                warn!("Could not set the duration of a media source: {error:?}");
            }

            if let Err(error) = self.with_player(|player| player.start_media_source()) {
                warn!("Could not start the playback of a media source: {error:?}");
                element.media_source_failed(MEDIA_ERR_DECODE, cx);
                return;
            }
            self.started.set(true);

            let has_audio = source_buffers
                .iter()
                .any(|source_buffer| source_buffer.has_audio());
            let video_size = source_buffers
                .iter()
                .find_map(|source_buffer| source_buffer.video_size());
            element.media_source_received_metadata(
                self.duration.get(),
                has_audio,
                video_size,
                CanGc::from_cx(cx),
            );
        }

        element.media_source_data_changed();
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
    ) -> DomRoot<MediaSource> {
        MediaSource::new_with_proto(window, proto, can_gc)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.active_source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> MediaSourceReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        // Step 1. If the readyState attribute is "closed" then return NaN.
        if self.ready_state.get() == MediaSourceReadyState::Closed {
            return f64::NAN;
        }

        // Step 2. Return the current value of the attribute.
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, duration: f64) -> ErrorResult {
        // Step 1. If the value being set is negative or NaN then throw a TypeError.
        if duration.is_nan() || duration < 0. {
            return Err(Error::Type(cformat!("Invalid duration: {}", duration)));
        }

        // Step 2. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 3. If the updating attribute equals true on any SourceBuffer in
        // sourceBuffers, then throw an InvalidStateError exception.
        if self.is_any_source_buffer_updating() {
            return Err(Error::InvalidState(None));
        }

        // Step 4. Run the duration change algorithm with new duration set to the value
        // being assigned to this attribute.
        self.change_duration(duration)
    }

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceopen
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceended
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceclose
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(&self, type_: DOMString, can_gc: CanGc) -> Fallible<DomRoot<SourceBuffer>> {
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(cformat!("The media type is empty")));
        }

        // Step 2. If type contains a MIME type that is not supported, then throw a
        // NotSupportedError exception.
        let media_type = String::from(type_);
        if !MediaSource::is_type_supported(&media_type) {
            return Err(Error::NotSupported(None));
        }

        // Step 4. If the readyState attribute is not in the "open" state then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 3. If the user agent can't handle any more SourceBuffer objects, then throw
        // a QuotaExceededError exception.
        // The media player takes no more source buffers once playback has started.
        let id = self
            .with_player(|player| player.add_source_buffer(&media_type))
            .map_err(|error| match error {
                PlayerError::QuotaExceeded => Error::QuotaExceeded {
                    quota: None,
                    requested: None,
                },
                PlayerError::UnsupportedMediaType => Error::NotSupported(None),
                _ => Error::InvalidState(None),
            })?;

        // Step 5. Create a new SourceBuffer object and associated resources.
        let source_buffer = SourceBuffer::new(self.global().as_window(), self, id, can_gc);

        // Step 9. Add the new object to sourceBuffers and queue a task to fire an event
        // named addsourcebuffer at sourceBuffers.
        self.source_buffers.add(&source_buffer);

        // Step 10. Return the new object.
        Ok(source_buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, source_buffer: &SourceBuffer) -> ErrorResult {
        // Step 1. If sourceBuffer specifies an object that is not in sourceBuffers then
        // throw a NotFoundError exception.
        if !self.source_buffers.contains(source_buffer) {
            return Err(Error::NotFound(None));
        }

        if let Err(error) =
            self.with_player(|player| player.remove_source_buffer(source_buffer.id()))
        {
            warn!("Could not remove a source buffer from the media player: {error:?}");
        }

        // Step 2. If the sourceBuffer.updating attribute equals true, then abort the
        // buffer append algorithm and fire the abort and updateend events.
        source_buffer.removed_from_media_source();

        // Step 10. If sourceBuffer is in activeSourceBuffers, then remove it and queue a
        // task to fire an event named removesourcebuffer at activeSourceBuffers.
        self.active_source_buffers.remove(source_buffer);

        // Step 11. Remove sourceBuffer from sourceBuffers and queue a task to fire an
        // event named removesourcebuffer at sourceBuffers.
        self.source_buffers.remove(source_buffer);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) -> ErrorResult {
        // Step 1. If the readyState attribute is not in the "open" state then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 2. If the updating attribute equals true on any SourceBuffer in
        // sourceBuffers, then throw an InvalidStateError exception.
        if self.is_any_source_buffer_updating() {
            return Err(Error::InvalidState(None));
        }

        // Step 3. Run the end of stream algorithm with the error parameter set to error.
        self.end_of_stream(error, cx);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        // Step 1. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 2. If start is negative or greater than end, then throw a TypeError
        // exception.
        if *start < 0. || *start > *end {
            return Err(Error::Type(cformat!(
                "Invalid live seekable range: {} to {}",
                *start,
                *end
            )));
        }

        // Step 3. Set live seekable range to be a new normalized TimeRanges object
        // containing a single range whose start position is start and end position is
        // end.
        self.live_seekable_range.set(Some(LiveSeekableRange {
            start: *start,
            end: *end,
        }));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        // Step 1. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }

        // Step 2. If live seekable range contains a range, then set live seekable range
        // to be a new empty TimeRanges object.
        self.live_seekable_range.set(None);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        MediaSource::is_type_supported(&type_.str())
    }
}
