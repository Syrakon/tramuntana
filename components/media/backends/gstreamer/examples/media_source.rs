/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Plays fragmented files through source buffers, the way a page does with
//! Media Source Extensions, and prints what the player reports.
//!
//! `media_source <file>... [--seek <seconds>]`
//!
//! Each file goes to a source buffer of its own, a segment every 300 ms.

use std::fmt::Debug;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use ipc_channel::ipc;
use servo_media::{BackendInit, ClientContextId};
use servo_media_gstreamer::GStreamerBackend;
use servo_media_player::context::{GlApi, GlContext, NativeDisplay, PlayerGLContext};
use servo_media_player::video::{VideoFrame, VideoFrameRenderer};
use servo_media_player::{PlayerEvent, SourceBufferId, StreamType};

struct NoGl;

impl PlayerGLContext for NoGl {
    fn get_gl_context(&self) -> GlContext {
        GlContext::Unknown
    }

    fn get_native_display(&self) -> NativeDisplay {
        NativeDisplay::Unknown
    }

    fn get_gl_api(&self) -> GlApi {
        GlApi::None
    }
}

/// What is behind these mutexes stays consistent if a thread panics while
/// holding one of them, so a poisoned lock is still usable.
fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(error: impl Debug) -> String {
    format!("{error:?}")
}

struct CountFrames(Arc<Mutex<(u32, i32, i32)>>);

impl VideoFrameRenderer for CountFrames {
    fn render(&mut self, frame: VideoFrame) {
        let mut count = lock(&self.0);
        *count = (count.0 + 1, frame.get_width(), frame.get_height());
    }
}

/// The initialization segment and the media segments of a file.
fn segments(path: &str, data: &[u8]) -> Vec<Vec<u8>> {
    if path.ends_with(".webm") {
        // A media segment starts with every Cluster element.
        let mut segments = Vec::new();
        let mut start = 0;
        for at in 0..data.len().saturating_sub(4) {
            if data[at..at + 4] == [0x1F, 0x43, 0xB6, 0x75] {
                segments.push(data[start..at].to_vec());
                start = at;
            }
        }
        segments.push(data[start..].to_vec());
        return segments;
    }

    // A media segment starts with every moof box.
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        // A box cannot be shorter than its header: a file that says so is cut here.
        let end = (at + (size as usize).max(8)).min(data.len());
        if &data[at + 4..at + 8] == b"moof" {
            segments.push(std::mem::take(&mut segment));
        }
        segment.extend_from_slice(&data[at..end]);
        at = end;
    }
    segments.push(segment);
    segments
}

fn run() -> Result<(), String> {
    let mut files = Vec::new();
    let mut seek = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--seek" {
            seek = args.next().and_then(|seconds| seconds.parse::<f64>().ok());
        } else {
            files.push(arg);
        }
    }

    let backend = GStreamerBackend::init();
    let (sender, receiver) = ipc::channel::<PlayerEvent>().map_err(failed)?;
    let frames = Arc::new(Mutex::new((0, 0, 0)));
    let player = backend.create_player(
        &ClientContextId::build(1, 1),
        StreamType::MediaSource,
        sender,
        Some(Arc::new(Mutex::new(CountFrames(frames.clone())))),
        None,
        Box::new(NoGl),
    );

    let start = Instant::now();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                PlayerEvent::VideoFrameUpdated => {},
                event => println!("[{:6.2}] {event:?}", start.elapsed().as_secs_f64()),
            }
        }
    });

    let mut pending: Vec<(SourceBufferId, std::vec::IntoIter<Vec<u8>>)> = Vec::new();
    {
        let player = lock(&player);
        player.set_mute(true).map_err(failed)?;
        for file in &files {
            let name = file.rsplit('/').next().unwrap_or(file);
            let media_type = match (file.ends_with(".webm"), name.starts_with('a')) {
                (true, true) => "audio/webm",
                (true, false) => "video/webm",
                (false, true) => "audio/mp4",
                (false, false) => "video/mp4",
            };
            let id = player.add_source_buffer(media_type).map_err(failed)?;
            let data = fs::read(file).map_err(|error| format!("{file}: {error}"))?;
            let mut segments = segments(file, &data).into_iter();
            let initialization = segments
                .next()
                .ok_or_else(|| format!("{file}: no initialization segment"))?;
            let state = player.append_data(id, initialization).map_err(failed)?;
            println!("{file}: {} segments, {state:?}", segments.len());
            pending.push((id, segments));
        }
        player.set_media_source_duration(Some(30.)).map_err(failed)?;
        player.start_media_source().map_err(failed)?;
        player.play().map_err(failed)?;
        if let Some(seek) = seek {
            println!("seek to {seek} before any media segment");
            player.seek(seek).map_err(failed)?;
        }
    }

    let mut round = 0;
    let mut sought_back = false;
    loop {
        thread::sleep(Duration::from_millis(300));
        round += 1;
        let player = lock(&player);
        let mut done = true;
        for (id, segments) in pending.iter_mut() {
            // With a seek, what comes before it is not appended, as a page would do.
            let skip = seek.map_or(0, |seek| (seek / 2.) as usize);
            let segment = if round == 1 {
                segments.nth(skip)
            } else {
                segments.next()
            };
            if let Some(segment) = segment {
                done = false;
                player.append_data(*id, segment).map_err(failed)?;
            }
        }
        if round % 10 == 0 {
            println!(
                "[{:6.2}] buffered {:?} seekable {:?} frames {:?}",
                start.elapsed().as_secs_f64(),
                player.buffered(),
                player.seekable(),
                lock(&frames)
            );
        }
        if done {
            player.set_media_source_ended(true).map_err(failed)?;
            if !sought_back && seek.is_none() && start.elapsed() > Duration::from_secs(6) {
                sought_back = true;
                println!("seek to 20 within buffered data");
                player.seek(20.).map_err(failed)?;
            }
        }
        if start.elapsed() > Duration::from_secs(22) {
            return Ok(());
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("media_source: {error}");
            ExitCode::FAILURE
        },
    }
}
