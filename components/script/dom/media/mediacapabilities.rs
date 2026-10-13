/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use mime::Mime;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_media::{ServoMedia, SupportsMediaType};

use crate::dom::bindings::codegen::Bindings::MediaCapabilitiesBinding::{
    MediaCapabilitiesDecodingInfo, MediaCapabilitiesMethods, MediaDecodingConfiguration,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediasource::MediaSource;
use crate::dom::promise::{Promise, RootedPromise};

/// <https://w3c.github.io/media-capabilities/#mediacapabilities>
#[dom_struct]
pub(crate) struct MediaCapabilities {
    reflector_: Reflector,
}

impl MediaCapabilities {
    fn new_inherited() -> MediaCapabilities {
        MediaCapabilities {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<MediaCapabilities> {
        reflect_dom_object(cx, Box::new(MediaCapabilities::new_inherited()), global)
    }
}

/// Whether the codec named in a `codecs` parameter is an audio one.
fn is_audio_codec(codec: &str) -> bool {
    let name = codec.split('.').next().unwrap_or(codec);
    matches!(
        name.to_ascii_lowercase().as_str(),
        "mp4a" | "opus" | "vorbis" | "flac" | "mp3" | "ac-3" | "ec-3" | "alac" | "samr" | "sawb"
    )
}

/// <https://w3c.github.io/media-capabilities/#check-mime-type-validity>
fn is_valid_content_type(content_type: &str, audio: bool) -> bool {
    // A valid MIME type string does not end with the separator of parameters, which the
    // parser lets through.
    if content_type.trim_end().ends_with(';') {
        return false;
    }
    let Ok(mime) = content_type.parse::<Mime>() else {
        return false;
    };
    let media = if audio { mime::AUDIO } else { mime::VIDEO };
    if mime.type_() != media && mime.type_() != mime::APPLICATION {
        return false;
    }
    if mime.params().any(|(name, _)| name.as_str() != "codecs") {
        return false;
    }
    let Some(codecs) = mime.get_param("codecs") else {
        // Without the parameter, the container has to tell the codec by itself.
        return audio && matches!(mime.subtype().as_str(), "mpeg" | "aac" | "flac" | "wav");
    };
    let mut codecs = codecs.as_str().split(',').map(str::trim);
    match (codecs.next(), codecs.next()) {
        (Some(codec), None) => !codec.is_empty() && is_audio_codec(codec) == audio,
        _ => false,
    }
}

impl MediaCapabilitiesMethods<crate::DomTypeHolder> for MediaCapabilities {
    /// <https://w3c.github.io/media-capabilities/#dom-mediacapabilities-decodinginfo>
    fn DecodingInfo(
        &self,
        cx: &mut CurrentRealm,
        configuration: &MediaDecodingConfiguration,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let video = configuration.parent.video.as_ref();
        let audio = configuration.parent.audio.as_ref();

        // <https://w3c.github.io/media-capabilities/#valid-mediadecodingconfiguration>
        let is_valid = (video.is_some() || audio.is_some()) &&
            video.is_none_or(|video| {
                *video.framerate > 0. && is_valid_content_type(&video.contentType.str(), false)
            }) &&
            audio.is_none_or(|audio| is_valid_content_type(&audio.contentType.str(), true));
        if !is_valid {
            promise.reject_error(
                cx,
                Error::Type(c"The media configuration is not valid".to_owned()),
            );
            return promise;
        }

        let decoding_type = configuration.type_.as_str();
        let is_supported = |content_type: &DOMString| match decoding_type {
            "file" => {
                ServoMedia::get().can_play_type(&content_type.str()) != SupportsMediaType::No
            },
            "media-source" => MediaSource::is_type_supported(&content_type.str()),
            // There is no decoding of media received over WebRTC yet.
            _ => false,
        };
        let supported = video.is_none_or(|video| is_supported(&video.contentType)) &&
            audio.is_none_or(|audio| is_supported(&audio.contentType));

        // The media player tells neither whether it keeps up with a resolution and frame
        // rate nor whether the decoder is a hardware one, so whatever it can play is said
        // to be smooth and power efficient.
        promise.resolve_native(
            cx,
            &MediaCapabilitiesDecodingInfo {
                supported,
                smooth: supported,
                powerEfficient: supported,
            },
        );
        promise
    }
}
