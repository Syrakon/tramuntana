/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! This module contains the `EventLoop` type, which is the constellation's
//! view of a script thread. When an `EventLoop` is dropped, an `ExitScriptThread`
//! message is sent to the script thread, asking it to shut down.

use std::hash::Hash;
use std::marker::PhantomData;
use std::rc::Rc;

use background_hang_monitor_api::{BackgroundHangMonitorControlMsg, HangAlert};
use embedder_traits::ScriptToEmbedderChan;
use layout_api::ScriptThreadFactory;
use log::error;
use media::WindowGLContext;
use script_traits::{InitialScriptState, ScriptThreadMessage};
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{GenericReceiver, GenericSender, SendError};
use servo_base::id::{ScriptEventLoopId, WebViewId};
use servo_url::Host;
#[cfg(feature = "multiprocess")]
use servo_config::opts::{self, Opts};
#[cfg(feature = "multiprocess")]
use servo_config::prefs::{self, Preferences};
use servo_constellation_traits::ServiceWorkerManagerFactory;

use crate::Constellation;
#[cfg(feature = "multiprocess")]
use crate::site_lock::SiteLock;

/// <https://html.spec.whatwg.org/multipage/#event-loop>
pub struct EventLoop {
    script_chan: GenericSender<ScriptThreadMessage>,
    id: ScriptEventLoopId,
    /// When running in another process, this is an `IpcSender` to the BackgroundHangMonitor
    /// on the other side of the process boundary. When running in the same process, the
    /// BackgroundHangMonitor is shared among all [`EventLoop`]s so this will be `None`.
    background_hang_monitor_sender: Option<GenericSender<BackgroundHangMonitorControlMsg>>,
    dont_send_or_sync: PhantomData<Rc<()>>,
}

impl PartialEq for EventLoop {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for EventLoop {}

impl Hash for EventLoop {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        self.send_message_to_background_hang_monitor(&BackgroundHangMonitorControlMsg::Exit);

        if let Err(error) = self.script_chan.send(ScriptThreadMessage::ExitScriptThread) {
            error!("Did not successfully request EventLoop exit: {error}");
        }
    }
}

impl EventLoop {
    pub(crate) fn spawn<STF: ScriptThreadFactory, SWF: ServiceWorkerManagerFactory>(
        constellation: &mut Constellation<STF, SWF>,
        webview_id: WebViewId,
        is_private: bool,
        site: Option<Host>,
        sandboxed: bool,
    ) -> Result<Rc<Self>, SendError> {
        let (script_chan, script_port) =
            servo_base::generic_channel::channel().expect("Pipeline script chan");

        let embedder_chan = constellation.embedder_proxy.sender.clone();
        let eventloop_waker = constellation.embedder_proxy.event_loop_waker.clone();
        let script_to_embedder_sender = ScriptToEmbedderChan::new(embedder_chan, eventloop_waker);

        // A webview in a temporary session uses the threads of that session
        // for everything it loads, the crash page included.
        let session_threads = constellation.temporary_session_threads(webview_id);
        let temporary_session = session_threads.is_some();
        let (resource_threads, storage_threads) =
            match session_threads {
                Some(session_threads) => session_threads,
                None if is_private => (
                    constellation.private_resource_threads.clone(),
                    constellation.private_storage_threads.clone(),
                ),
                None => (
                    constellation.public_resource_threads.clone(),
                    constellation.public_storage_threads.clone(),
                ),
            };

        let event_loop_id = ScriptEventLoopId::new();
        let initial_script_state = InitialScriptState {
            id: event_loop_id,
            script_to_constellation_sender: constellation.script_sender.clone(),
            script_to_embedder_sender,
            namespace_request_sender: constellation.namespace_ipc_sender.clone(),
            devtools_server_sender: constellation.script_to_devtools_callback(),
            #[cfg(feature = "bluetooth")]
            bluetooth_sender: constellation.bluetooth_ipc_sender.clone(),
            system_font_service: constellation.system_font_service.to_sender(),
            resource_threads,
            storage_threads,
            time_profiler_sender: constellation.time_profiler_chan.clone(),
            memory_profiler_sender: constellation.mem_profiler_chan.clone(),
            constellation_to_script_sender: script_chan,
            constellation_to_script_receiver: script_port,
            pipeline_namespace_id: constellation.next_pipeline_namespace_id(),
            cross_process_paint_api: constellation.paint_proxy.cross_process_paint_api.clone(),
            // Without the channel no WebGL context can be made, and a page of a
            // temporary session is not to learn what the graphics card is.
            #[cfg(feature = "webgl")]
            webgl_chan: constellation
                .webgl_threads
                .as_ref()
                .filter(|_| !temporary_session)
                .map(|threads| threads.pipeline()),
            webxr_registry: constellation.webxr_registry.clone(),
            player_context: WindowGLContext::get(),
            privileged_urls: constellation.privileged_urls.clone(),
            temporary_session,
            user_contents_for_manager_id: constellation.user_contents_for_manager_id.clone(),
        };

        #[cfg(feature = "multiprocess")]
        let event_loop = if opts::get().multiprocess {
            Self::spawn_in_process(
                constellation,
                initial_script_state,
                SiteLock::new(site, sandboxed),
            )?
        } else {
            Self::spawn_in_thread(constellation, initial_script_state)
        };
        #[cfg(not(feature = "multiprocess"))]
        let event_loop = {
            // Script running in this process shares its memory: there is no site to lock it to.
            let _ = (site, sandboxed);
            Self::spawn_in_thread(constellation, initial_script_state)
        };

        let event_loop = Rc::new(event_loop);
        constellation.add_event_loop(&event_loop);
        Ok(event_loop)
    }

    fn spawn_in_thread<STF: ScriptThreadFactory, SWF: ServiceWorkerManagerFactory>(
        constellation: &mut Constellation<STF, SWF>,
        initial_script_state: InitialScriptState,
    ) -> Self {
        let script_chan = initial_script_state.constellation_to_script_sender.clone();
        let id = initial_script_state.id;
        let background_hang_monitor_register = constellation
            .background_monitor_register
            .clone()
            .expect("Couldn't start content, no background monitor has been initiated");
        let join_handle = STF::create(
            initial_script_state,
            constellation.layout_factory.clone(),
            constellation.image_cache_factory.clone(),
            background_hang_monitor_register,
        );
        constellation.add_event_loop_join_handle(join_handle);

        Self {
            script_chan,
            id,
            background_hang_monitor_sender: None,
            dont_send_or_sync: PhantomData,
        }
    }

    #[cfg(feature = "multiprocess")]
    fn spawn_in_process<STF: ScriptThreadFactory, SWF: ServiceWorkerManagerFactory>(
        constellation: &mut Constellation<STF, SWF>,
        mut initial_script_state: InitialScriptState,
        site_lock: SiteLock,
    ) -> Result<Self, SendError> {
        let script_chan = initial_script_state.constellation_to_script_sender.clone();
        let id = initial_script_state.id;

        // The process gets channels of its own to the constellation and to the resource and
        // storage threads, which know what it runs and refuse what it may not do.
        let (script_to_constellation_sender, script_to_constellation_receiver) =
            servo_base::generic_channel::channel().ok_or(SendError::Disconnected)?;
        let script_from_process_sender = constellation.script_from_process_sender.clone();
        script_to_constellation_receiver.route_with(move |message| {
            let _ = script_from_process_sender.send(message.map(
                |(webview_id, pipeline_id, message)| (Some(id), webview_id, pipeline_id, message),
            ));
        });
        initial_script_state.script_to_constellation_sender = script_to_constellation_sender;
        let owned_namespaces = crate::site_lock::OwnedNamespaces::default();
        owned_namespaces
            .lock()
            .insert(initial_script_state.pipeline_namespace_id);
        initial_script_state.namespace_request_sender = crate::site_lock::lock_namespace_requests(
            owned_namespaces.clone(),
            initial_script_state.namespace_request_sender.clone(),
        )
        .ok_or(SendError::Disconnected)?;
        let (resource_threads, storage_threads) = crate::site_lock::lock_threads(
            site_lock,
            owned_namespaces,
            &initial_script_state.resource_threads,
            &initial_script_state.storage_threads,
        )
        .ok_or(SendError::Disconnected)?;
        initial_script_state.resource_threads = resource_threads;
        initial_script_state.storage_threads = storage_threads;

        let (background_hand_monitor_sender, backgrond_hand_monitor_receiver) =
            servo_base::generic_channel::channel().expect("Sampler chan");
        let (lifeline_sender, lifeline_receiver) =
            servo_base::generic_channel::channel().expect("Failed to create lifeline channel");

        let process = crate::sandboxing::spawn_multiprocess(
            crate::UnprivilegedContent::ScriptEventLoop(NewScriptEventLoopProcessInfo {
                initial_script_state,
                constellation_to_bhm_receiver: backgrond_hand_monitor_receiver,
                bhm_to_constellation_sender: constellation.background_hang_monitor_sender.clone(),
                lifeline_sender,
                opts: (*opts::get()).clone(),
                prefs: Box::new(prefs::get().clone()),
                broken_image_icon_data: constellation.broken_image_icon_data.clone(),
            }),
        )?;

        let crossbeam_receiver = lifeline_receiver.route_preserving_errors();
        constellation
            .process_manager
            .add(crossbeam_receiver, process);

        Ok(Self {
            script_chan,
            id,
            background_hang_monitor_sender: Some(background_hand_monitor_sender),
            dont_send_or_sync: PhantomData,
        })
    }

    pub(crate) fn id(&self) -> ScriptEventLoopId {
        self.id
    }

    /// Send a message to the event loop.
    pub fn send(&self, msg: ScriptThreadMessage) -> Result<(), SendError> {
        self.script_chan.send(msg)
    }

    /// If this is [`EventLoop`] is in another process, send a message to its `BackgroundHangMonitor`,
    /// otherwise do nothing.
    pub(crate) fn send_message_to_background_hang_monitor(
        &self,
        message: &BackgroundHangMonitorControlMsg,
    ) {
        if let Some(background_hang_monitor_sender) = &self.background_hang_monitor_sender &&
            let Err(error) = background_hang_monitor_sender.send(message.clone())
        {
            error!("Could not send message ({message:?}) to BHM: {error}");
        }
    }
}

/// All of the information necessary to create a new script [`EventLoop`] in a new process.
#[derive(Deserialize, Serialize)]
#[cfg(feature = "multiprocess")]
pub struct NewScriptEventLoopProcessInfo {
    pub initial_script_state: InitialScriptState,
    pub constellation_to_bhm_receiver: GenericReceiver<BackgroundHangMonitorControlMsg>,
    pub bhm_to_constellation_sender: GenericSender<HangAlert>,
    pub lifeline_sender: GenericSender<()>,
    pub opts: Opts,
    pub prefs: Box<Preferences>,
    /// The broken image icon data that is used to create an image to show in place of broken images.
    pub broken_image_icon_data: Vec<u8>,
}
