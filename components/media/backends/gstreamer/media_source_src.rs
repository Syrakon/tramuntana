/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use url::Url;

// Implementation sub-module of the GObject
mod imp {
    use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

    use super::*;

    /// Empty if the template could not be created, which leaves the element
    /// without pads and makes the pipeline fail to start.
    static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
        gstreamer::PadTemplate::new(
            "src_%u",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &gstreamer::Caps::new_any(),
        )
        .into_iter()
        .collect()
    });

    #[derive(Default)]
    struct Tracks {
        appsrcs: Vec<gstreamer_app::AppSrc>,
        exposed: bool,
        /// The seek being carried out or the last one that was.
        last_seek: Option<gstreamer::Seqnum>,
    }

    pub struct ServoMediaSourceSrc {
        cat: gstreamer::DebugCategory,
        tracks: Mutex<Tracks>,
    }

    impl ServoMediaSourceSrc {
        fn tracks(&self) -> MutexGuard<'_, Tracks> {
            // The track list stays consistent even if a thread panicked while holding it.
            self.tracks.lock().unwrap_or_else(PoisonError::into_inner)
        }

        pub fn add_track(&self, appsrc: &gstreamer_app::AppSrc) -> Result<(), glib::BoolError> {
            self.obj().add(appsrc)?;
            self.tracks().appsrcs.push(appsrc.clone());
            Ok(())
        }

        /// Give every track a source pad. urisourcebin only looks for the pads of a
        /// source with more than one of them once it has been told they are coming,
        /// and the tracks must be linked before their appsrc starts pushing.
        fn expose_tracks(&self) -> Result<(), glib::BoolError> {
            let template = PAD_TEMPLATES
                .first()
                .ok_or_else(|| glib::bool_error!("No src pad template"))?;
            let appsrcs = {
                let mut tracks = self.tracks();
                if tracks.exposed {
                    return Ok(());
                }
                tracks.exposed = true;
                tracks.appsrcs.clone()
            };

            for (index, appsrc) in appsrcs.iter().enumerate() {
                let target = appsrc
                    .static_pad("src")
                    .ok_or_else(|| glib::bool_error!("appsrc has no src pad"))?;
                let pad = gstreamer::GhostPad::builder_from_template(template)
                    .name(format!("src_{index}"))
                    .event_function(|pad, parent, event| {
                        ServoMediaSourceSrc::catch_panic_pad_function(
                            parent,
                            || false,
                            |this| this.src_event(pad, event),
                        )
                    })
                    .build();
                pad.set_target(Some(&target))?;
                pad.set_active(true)?;
                self.obj().add_pad(&pad)?;
                gstreamer::debug!(self.cat, obj = &pad, "Exposed track {index}");
            }
            self.obj().no_more_pads();
            Ok(())
        }
    }

    impl ServoMediaSourceSrc {
        fn src_event(&self, pad: &gstreamer::GhostPad, event: gstreamer::Event) -> bool {
            if event.type_() != gstreamer::EventType::Seek {
                return gstreamer::Pad::event_default(pad, Some(&*self.obj()), event);
            }

            // A seek arrives through one of the tracks, and through that one only
            // when the sinks expect a single demuxer to move every stream. The
            // tracks here are sources of their own, so each has to be told.
            let appsrcs = {
                let mut tracks = self.tracks();
                if tracks.last_seek == Some(event.seqnum()) {
                    return true;
                }
                tracks.last_seek = Some(event.seqnum());
                tracks.appsrcs.clone()
            };
            gstreamer::debug!(self.cat, obj = pad, "Seeking every track: {event:?}");
            appsrcs
                .iter()
                .fold(true, |done, appsrc| appsrc.send_event(event.clone()) && done)
        }
    }

    // Basic declaration of our type for the GObject type system
    #[glib::object_subclass]
    impl ObjectSubclass for ServoMediaSourceSrc {
        const NAME: &'static str = "ServoMediaSourceSrc";
        type Type = super::ServoMediaSourceSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        fn new() -> Self {
            Self {
                cat: gstreamer::DebugCategory::new(
                    "servomediasourcesrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo media source"),
                ),
                tracks: Mutex::new(Tracks::default()),
            }
        }
    }

    impl ObjectImpl for ServoMediaSourceSrc {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoMediaSourceSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoMediaSourceSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Source Extensions Source",
                        "Source/Audio/Video",
                        "Feed player with the coded frames of source buffers",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            PAD_TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gstreamer::StateChange,
        ) -> Result<gstreamer::StateChangeSuccess, gstreamer::StateChangeError> {
            if transition == gstreamer::StateChange::ReadyToPaused {
                self.expose_tracks().map_err(|error| {
                    gstreamer::error!(self.cat, imp = self, "Could not expose tracks: {error}");
                    gstreamer::StateChangeError
                })?;
            }
            self.parent_change_state(transition)
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoMediaSourceSrc {}

    impl URIHandlerImpl for ServoMediaSourceSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servomediasource"]
        }

        fn uri(&self) -> Option<String> {
            Some("servomediasource://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri) &&
                uri.scheme() == "servomediasource"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoMediaSourceSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoMediaSourceSrc(ObjectSubclass<imp::ServoMediaSourceSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

impl ServoMediaSourceSrc {
    /// Add the appsrc of a track. Every track must be added before the element
    /// leaves the READY state.
    pub fn add_track(&self, appsrc: &gstreamer_app::AppSrc) -> Result<(), glib::BoolError> {
        self.imp().add_track(appsrc)
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servomediasourcesrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_media_source_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomediasourcesrc",
        gstreamer::Rank::NONE,
        ServoMediaSourceSrc::static_type(),
    )
}
