/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use js::rust::{JSEngine, JSEngineHandle, ParentRuntime, Runtime as RustRuntime};
use log::warn;
use profile_traits::mem::{
    ProcessReports, ProfilerChan, ProfilerMsg, Report, Reporter, ReporterRequest, ReportsChan,
    perform_memory_report,
};
use servo_base::generic_channel::{GenericCallback, SendError};

use crate::runtime::script_runtime::get_reports;

static JS_ENGINE: Mutex<Option<JSEngineHandle>> = Mutex::new(None);

/// The way to the thread that owns this process's parent runtime. It is the only sender, so
/// taking it out of here is what tells that thread to wind down.
static PARENT_RUNTIME: Mutex<Option<Sender<ParentRuntimeRequest>>> = Mutex::new(None);

/// Runtimes created as children of the parent runtime that have not been destroyed yet,
/// counting the [`ParentRuntime`] handles on their way to becoming one.
static LIVE_CHILD_RUNTIMES: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn current_js_engine_handle() -> JSEngineHandle {
    JS_ENGINE.lock().unwrap().as_ref().unwrap().clone()
}

enum ParentRuntimeRequest {
    NewChild(Sender<(ParentRuntime, ChildRuntimeToken)>),
    CollectReports(ReportsChan),
}

/// Keeps the parent runtime alive while a child of it exists. The child runtime has to be
/// destroyed before this token is dropped.
pub(crate) struct ChildRuntimeToken(());

impl ChildRuntimeToken {
    fn new() -> Self {
        LIVE_CHILD_RUNTIMES.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for ChildRuntimeToken {
    fn drop(&mut self) {
        LIVE_CHILD_RUNTIMES.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A handle to create a runtime as a child of this process's parent runtime, or `None` when
/// the process has no parent runtime and the new runtime has to stand alone.
///
/// A child shares the parent's self-hosted code and permanent atoms instead of building its
/// own copy, and it leaves the process-wide caches (script sources, shared bytecode) to the
/// parent's memory report, which every standalone runtime would otherwise count again.
pub(crate) fn new_child_of_parent_runtime() -> Option<(ParentRuntime, ChildRuntimeToken)> {
    let (reply_sender, reply_receiver) = crossbeam_channel::bounded(1);
    send_to_parent_runtime(ParentRuntimeRequest::NewChild(reply_sender)).ok()?;
    reply_receiver.recv().ok()
}

fn send_to_parent_runtime(request: ParentRuntimeRequest) -> Result<(), ParentRuntimeRequest> {
    let parent = PARENT_RUNTIME
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match parent.as_ref() {
        Some(sender) => sender.send(request).map_err(|error| error.into_inner()),
        None => Err(request),
    }
}

/// Register the memory reporter of this process's parent runtime, which is where the
/// process-wide JS caches are counted, once.
pub fn register_parent_runtime_memory_reporter(memory_profiler_sender: &ProfilerChan) {
    let callback = GenericCallback::new(|request: Result<ReporterRequest, SendError>| {
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                warn!("Could not receive a memory report request for the parent runtime: {error}");
                return;
            },
        };
        let request = ParentRuntimeRequest::CollectReports(request.reports_channel);
        if let Err(ParentRuntimeRequest::CollectReports(reports_channel)) =
            send_to_parent_runtime(request)
        {
            // The profiler waits for an answer from every reporter.
            send_reports(&reports_channel, vec![]);
        }
    });
    match callback {
        Ok(callback) => memory_profiler_sender.send(ProfilerMsg::RegisterReporter(
            format!("parent-js-runtime-{}", std::process::id()),
            Reporter(callback),
        )),
        Err(error) => warn!("Could not register the parent runtime memory reporter: {error}"),
    }
}

fn send_reports(reports_channel: &ReportsChan, reports: Vec<Report>) {
    reports_channel.0.send_or_warn(ProcessReports::new(reports));
}

fn run_parent_runtime(engine: JSEngineHandle, requests: Receiver<ParentRuntimeRequest>) {
    let mut runtime = RustRuntime::new(engine);

    for request in requests {
        match request {
            ParentRuntimeRequest::NewChild(reply) => {
                let child = (runtime.prepare_for_new_child(), ChildRuntimeToken::new());
                if reply.send(child).is_err() {
                    warn!("A script runtime asked for the parent runtime and stopped waiting");
                }
            },
            ParentRuntimeRequest::CollectReports(reports_channel) => {
                let mut reports = vec![];
                perform_memory_report(|ops| {
                    reports = get_reports(
                        runtime.cx(),
                        "parent-js-runtime".to_owned(),
                        ops,
                        HashSet::new(),
                    );
                });
                send_reports(&reports_channel, reports);
            },
        }
    }

    // The children borrow this runtime's atoms and self-hosted code, so it goes last.
    while LIVE_CHILD_RUNTIMES.load(Ordering::SeqCst) != 0 {
        thread::sleep(Duration::from_millis(50));
    }
    drop(runtime);
}

pub struct JSEngineSetup {
    engine: JSEngine,
    parent_runtime: Option<JoinHandle<()>>,
}

impl Default for JSEngineSetup {
    fn default() -> Self {
        let engine = JSEngine::init().unwrap();
        *JS_ENGINE.lock().unwrap() = Some(engine.handle());

        let (request_sender, request_receiver) = crossbeam_channel::unbounded();
        let parent_engine = engine.handle();
        let parent_runtime = thread::Builder::new()
            .name("JSParentRuntime".to_owned())
            .spawn(move || run_parent_runtime(parent_engine, request_receiver));
        let parent_runtime = match parent_runtime {
            Ok(join_handle) => {
                *PARENT_RUNTIME
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(request_sender);
                Some(join_handle)
            },
            Err(error) => {
                warn!("Could not start the parent runtime, script runtimes will stand alone: {error}");
                None
            },
        };

        Self {
            engine,
            parent_runtime,
        }
    }
}

impl Drop for JSEngineSetup {
    fn drop(&mut self) {
        *JS_ENGINE.lock().unwrap() = None;

        PARENT_RUNTIME
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(parent_runtime) = self.parent_runtime.take() &&
            parent_runtime.join().is_err()
        {
            warn!("The parent runtime thread panicked");
        }

        while !self.engine.can_shutdown() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}
