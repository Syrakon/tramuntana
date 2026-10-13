/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The thread on which players are let go of.
//!
//! The last reference to a `gstreamer_play::Play` must not be given up on a thread that
//! posts the messages of that player: `gst_play_dispose` would then run on the thread it
//! has to wait for, and that thread would go on with a player that is freed. Every
//! message of a player holds a reference to it, so a thread that posts one and frees it
//! afterwards can be the one that gives up the last reference.
//!
//! So the messages stay on the bus of the player after they are handled, and this thread
//! takes them from it. The player itself is given up here too, which also keeps the wait
//! for its thread out of the script thread.

use std::any::Any;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How often the bus of a player that is going away is looked at.
const LEAVING_POLL: Duration = Duration::from_millis(20);

/// How long a player that is going away is waited for.
const LEAVING_LIMIT: Duration = Duration::from_secs(30);

enum Retired {
    /// A bus with messages that have been handled.
    Messages(gstreamer::Bus),
    /// Something that keeps a player alive.
    Owner(Box<dyn Any + Send>),
    /// What a media element kept its player alive with. The player may outlive it for
    /// as long as a message of it is on its way.
    Player {
        owner: Box<dyn Any + Send>,
        bus: gstreamer::Bus,
        play: glib::WeakRef<gstreamer_play::Play>,
    },
}

/// A player that nothing but its own messages keeps alive.
struct Leaving {
    bus: gstreamer::Bus,
    play: glib::WeakRef<gstreamer_play::Play>,
    since: Instant,
}

fn sender() -> Option<&'static Sender<Retired>> {
    static SENDER: OnceLock<Option<Sender<Retired>>> = OnceLock::new();
    SENDER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("GStreamerReaper".to_owned())
                .spawn(move || run(receiver))
                .map_err(|error| log::warn!("Could not start the reaper thread: {error}"))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Hands `retired` to the thread, or gives it back if there is no thread.
fn retire(retired: Retired) -> Result<(), Retired> {
    match sender() {
        Some(sender) => sender.send(retired).map_err(|error| error.0),
        None => Err(retired),
    }
}

/// Lets go of `owner`, which keeps a player alive, on the reaper thread.
pub(crate) fn let_go(owner: Box<dyn Any + Send>) {
    // Without the thread it is dropped here, which is what would have happened anyway.
    drop(retire(Retired::Owner(owner)));
}

/// Lets go of `owner`, the reference a media element had to the player `play`, and sees
/// that the messages left on `bus` do not keep the player alive.
pub(crate) fn let_go_of_player(
    owner: Box<dyn Any + Send>,
    bus: gstreamer::Bus,
    play: glib::WeakRef<gstreamer_play::Play>,
) {
    if let Err(retired) = retire(Retired::Player { owner, bus, play }) {
        // The messages on the bus hold the player, and nothing will take them from it.
        if let Retired::Player { bus, .. } = &retired {
            bus.set_flushing(true);
        }
    }
}

/// What the handler of the messages of a player answers once it is done with a message:
/// the message stays on `bus` until the reaper thread takes it.
pub(crate) fn keep_message(bus: &gstreamer::Bus) -> gstreamer::BusSyncReply {
    match retire(Retired::Messages(bus.clone())) {
        Ok(()) => gstreamer::BusSyncReply::Pass,
        Err(_) => gstreamer::BusSyncReply::Drop,
    }
}

fn drain(bus: &gstreamer::Bus) {
    while let Some(message) = bus.pop() {
        drop(message);
    }
}

fn run(receiver: Receiver<Retired>) {
    let mut leaving: Vec<Leaving> = Vec::new();
    loop {
        let retired = if leaving.is_empty() {
            match receiver.recv() {
                Ok(retired) => Some(retired),
                Err(_) => return,
            }
        } else {
            match receiver.recv_timeout(LEAVING_POLL) {
                Ok(retired) => Some(retired),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        match retired {
            Some(Retired::Messages(bus)) => drain(&bus),
            Some(Retired::Owner(owner)) => drop(owner),
            Some(Retired::Player { owner, bus, play }) => {
                drop(owner);
                leaving.push(Leaving {
                    bus,
                    play,
                    since: Instant::now(),
                });
            },
            None => {},
        }

        leaving.retain(|leaving| {
            // A message that was on its way when the player was let go of is the last
            // thing that holds it.
            drain(&leaving.bus);
            if leaving.play.upgrade().is_none() {
                return false;
            }
            if leaving.since.elapsed() > LEAVING_LIMIT {
                log::warn!("A media player is still alive long after it was let go of");
                return false;
            }
            true
        });
    }
}
