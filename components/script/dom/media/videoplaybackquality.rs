/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::PerformanceBinding::DOMHighResTimeStamp;
use crate::dom::bindings::codegen::Bindings::VideoPlaybackQualityBinding::VideoPlaybackQualityMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::window::Window;

/// <https://w3c.github.io/media-playback-quality/#videoplaybackquality>
#[dom_struct]
pub(crate) struct VideoPlaybackQuality {
    reflector_: Reflector,
    creation_time: DOMHighResTimeStamp,
    dropped_video_frames: u32,
    total_video_frames: u32,
}

impl VideoPlaybackQuality {
    fn new_inherited(
        creation_time: DOMHighResTimeStamp,
        dropped_video_frames: u32,
        total_video_frames: u32,
    ) -> VideoPlaybackQuality {
        VideoPlaybackQuality {
            reflector_: Reflector::new(),
            creation_time,
            dropped_video_frames,
            total_video_frames,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        creation_time: DOMHighResTimeStamp,
        dropped_video_frames: u32,
        total_video_frames: u32,
    ) -> DomRoot<VideoPlaybackQuality> {
        reflect_dom_object(
            cx,
            Box::new(VideoPlaybackQuality::new_inherited(
                creation_time,
                dropped_video_frames,
                total_video_frames,
            )),
            window,
        )
    }
}

impl VideoPlaybackQualityMethods<crate::DomTypeHolder> for VideoPlaybackQuality {
    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-creationtime>
    fn CreationTime(&self) -> DOMHighResTimeStamp {
        self.creation_time
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-droppedvideoframes>
    fn DroppedVideoFrames(&self) -> u32 {
        self.dropped_video_frames
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-totalvideoframes>
    fn TotalVideoFrames(&self) -> u32 {
        self.total_video_frames
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-corruptedvideoframes>
    fn CorruptedVideoFrames(&self) -> u32 {
        // The media player does not tell corrupted frames from the rest.
        0
    }
}
