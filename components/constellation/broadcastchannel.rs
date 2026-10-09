/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;

use log::warn;
use rustc_hash::FxHashMap;
use servo_base::generic_channel::GenericCallback;
use servo_base::id::BroadcastChannelRouterId;
use servo_constellation_traits::BroadcastChannelMsg;
use servo_url::ImmutableOrigin;

#[derive(Default)]
pub(crate) struct BroadcastChannels {
    /// A map of broadcast routers to their Generic sender.
    routers: FxHashMap<BroadcastChannelRouterId, GenericCallback<BroadcastChannelMsg>>,

    /// A map of origin to a map of channel name to a list of relevant routers.
    /// The origin comes with the temporary session of the page, if it is in one:
    /// a channel does not reach from one session into another, nor into the
    /// pages outside any.
    channels: HashMap<ChannelScope, HashMap<String, Vec<BroadcastChannelRouterId>>>,
}

/// Where a channel name is heard: an origin, within a temporary session or outside
/// them all.
type ChannelScope = (Option<u64>, ImmutableOrigin);

impl BroadcastChannels {
    /// Add a new broadcast router.
    #[servo_tracing::instrument(skip_all)]
    pub fn new_broadcast_channel_router(
        &mut self,
        router_id: BroadcastChannelRouterId,
        broadcast_callback: GenericCallback<BroadcastChannelMsg>,
    ) {
        if self.routers.insert(router_id, broadcast_callback).is_some() {
            warn!("Multiple attempts to add BroadcastChannel router.");
        }
    }

    /// Remove a broadcast router.
    #[servo_tracing::instrument(skip_all)]
    pub fn remove_broadcast_channel_router(&mut self, router_id: BroadcastChannelRouterId) {
        if self.routers.remove(&router_id).is_none() {
            warn!("Attempt to remove unknown BroadcastChannel router.");
        }
        // Also remove the router_id from the broadcast_channels list.
        for channels in self.channels.values_mut() {
            for routers in channels.values_mut() {
                routers.retain(|router| router != &router_id);
            }
        }
    }

    /// Note a new channel-name relevant to a given broadcast router.
    #[servo_tracing::instrument(skip_all)]
    pub fn new_broadcast_channel_name_in_router(
        &mut self,
        router_id: BroadcastChannelRouterId,
        channel_name: String,
        origin: ImmutableOrigin,
        session: Option<u64>,
    ) {
        let channels = self.channels.entry((session, origin)).or_default();
        let routers = channels.entry(channel_name).or_default();
        routers.push(router_id);
    }

    /// Remove a channel-name for a given broadcast router.
    #[servo_tracing::instrument(skip_all)]
    pub fn remove_broadcast_channel_name_in_router(
        &mut self,
        router_id: BroadcastChannelRouterId,
        channel_name: String,
        origin: ImmutableOrigin,
        session: Option<u64>,
    ) {
        let origin = (session, origin);
        if let Some(channels) = self.channels.get_mut(&origin) {
            let is_empty = if let Some(routers) = channels.get_mut(&channel_name) {
                routers.retain(|router| router != &router_id);
                routers.is_empty()
            } else {
                return warn!(
                    "Multiple attempts to remove name for BroadcastChannel {:?} at {:?}",
                    channel_name, origin
                );
            };
            if is_empty {
                channels.remove(&channel_name);
            }
        } else {
            warn!(
                "Attempt to remove a channel name for an origin without channels {:?}",
                origin
            );
        }
    }

    /// Broadcast a message via routers in various event-loops.
    #[servo_tracing::instrument(skip_all)]
    pub fn schedule_broadcast(
        &self,
        router_id: BroadcastChannelRouterId,
        message: BroadcastChannelMsg,
        session: Option<u64>,
    ) {
        if let Some(channels) = self.channels.get(&(session, message.origin.clone())) {
            let routers = match channels.get(&message.channel_name) {
                Some(routers) => routers,
                None => return warn!("Broadcast to channel name without active routers."),
            };
            for router in routers {
                // Exclude the sender of the broadcast.
                // Broadcasting locally is done at the point of sending.
                if router == &router_id {
                    continue;
                }

                if let Some(broadcast_ipc_sender) = self.routers.get(router) {
                    if broadcast_ipc_sender.send(message.clone()).is_err() {
                        warn!("Failed to broadcast message to router: {:?}", router);
                    }
                } else {
                    warn!("No sender for broadcast router: {:?}", router);
                }
            }
        } else {
            warn!(
                "Attempt to schedule a broadcast for an origin without routers {:?}",
                message.origin
            );
        }
    }
}
