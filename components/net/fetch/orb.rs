/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Opaque Response Blocking: a cross-origin `no-cors` response reaches the
//! content process only when it can be an image, media, a script or a
//! stylesheet. <https://github.com/annevk/orb>
//!
//! The proposal ends by parsing the whole body as JavaScript. This does not:
//! it sniffs the first [`SNIFF_LEN`] bytes for HTML, XML and JSON instead, so
//! the network process needs no JavaScript engine and a response waits only
//! for its first bytes, not for all of them.

use std::collections::VecDeque;

use http::HeaderMap;
use http::header::CONTENT_RANGE;
use log::debug;
use mime::Mime;
use net_traits::fetch::headers::{determine_nosniff, extract_mime_type_as_mime};
use net_traits::mime_classifier::MimeClassifier;
use net_traits::response::{Response, ResponseBody};
use parking_lot::RwLock;
use servo_url::ServoUrl;

use crate::fetch::methods::{Data, DoneChannel};

/// How many bytes of the body are looked at before deciding.
pub const SNIFF_LEN: usize = 1024;

/// How many media URLs are remembered for later range requests.
const MEDIA_URLS_KEPT: usize = 256;

/// MIME types that are never a script, a stylesheet, an image or media, so
/// their responses are blocked without looking at the body.
const NEVER_SNIFFED: &[&str] = &[
    "application/gzip",
    "application/x-gzip",
    "application/zip",
    "application/x-zip-compressed",
    "application/pdf",
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.ms-powerpoint",
    "application/x-protobuf",
    "application/x-protobuffer",
    "multipart/byteranges",
    "multipart/signed",
    "text/event-stream",
    "text/csv",
    "text/vtt",
];

/// Prefixes servers put in front of JSON so that it cannot run as a script.
const NON_SCRIPT_PREFIXES: &[&[u8]] = &[
    b")]}'",
    b"{}&&",
    b"{} &&",
    b"for(;;);",
    b"for (;;);",
    b"while(1);",
    b"while (1);",
];

/// Tags an HTML document starts with, matched without regard to case.
const HTML_TAGS: &[&[u8]] = &[
    b"<!doctype html",
    b"<html",
    b"<head",
    b"<script",
    b"<body",
    b"<iframe",
    b"<div",
    b"<table",
    b"<title",
    b"<style",
    b"<p",
    b"<a",
    b"<b",
    b"<br",
    b"<h1",
    b"<font",
];

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow,
    /// Allowed, and the URL gave audio or video, so later range requests for
    /// it are allowed too.
    AllowMedia,
    Block,
}

/// The URLs that recently gave audio or video to a `no-cors` request.
///
/// A range request that does not start at the first byte cannot be sniffed,
/// so it is allowed only for a URL that already gave media. The destination
/// of the request comes from the content process and is not trusted for this.
#[derive(Default)]
pub struct MediaUrls(VecDeque<ServoUrl>);

impl MediaUrls {
    pub fn contains(&self, url: &ServoUrl) -> bool {
        self.0.contains(url)
    }

    pub fn insert(&mut self, url: ServoUrl) {
        if self.contains(&url) {
            return;
        }
        if self.0.len() == MEDIA_URLS_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(url);
    }
}

/// Decides from the status and the headers alone, or returns `None` when the
/// body has to be looked at.
pub fn check_headers(headers: &HeaderMap, status: u16, known_media: bool) -> Option<Verdict> {
    if let Some(mime) = extract_mime_type_as_mime(headers) {
        if MimeClassifier::is_javascript(&mime) ||
            MimeClassifier::is_css(&mime) ||
            mime.essence_str() == "image/svg+xml"
        {
            return Some(Verdict::Allow);
        }
        if is_never_sniffed(&mime) {
            return Some(Verdict::Block);
        }
    }
    if status == 206 && !range_starts_at_zero(headers) {
        return Some(if known_media {
            Verdict::Allow
        } else {
            Verdict::Block
        });
    }
    None
}

/// Decides from the first [`SNIFF_LEN`] bytes of the body.
pub fn check_body(headers: &HeaderMap, head: &[u8], classifier: &MimeClassifier) -> Verdict {
    if classifier.sniffs_as_audio_video(head) {
        return Verdict::AllowMedia;
    }
    if classifier.sniffs_as_image(head) {
        return Verdict::Allow;
    }
    let declared_document = extract_mime_type_as_mime(headers).is_some_and(|mime| {
        MimeClassifier::is_html(&mime) ||
            MimeClassifier::is_xml(&mime) ||
            MimeClassifier::is_json(&mime) ||
            mime.essence_str() == "text/plain"
    });
    if declared_document &&
        (determine_nosniff(headers) ||
            looks_like_html(head) ||
            looks_like_xml(head) ||
            looks_like_json_object(head))
    {
        return Verdict::Block;
    }
    if has_non_script_prefix(head) || looks_like_json_object(head) {
        return Verdict::Block;
    }
    Verdict::Allow
}

/// Checks an opaque response before the content process sees it, and tells
/// whether it may go there. The body data read to decide is left in `head`,
/// in order, for the caller to hand on before the rest.
pub(crate) async fn check_response(
    response: &Response,
    done_chan: &mut DoneChannel,
    head: &mut VecDeque<Data>,
    media_urls: &RwLock<MediaUrls>,
) -> bool {
    let Some(internal) = response.internal_response.as_deref() else {
        return false;
    };
    let url = internal.url_list.last();
    let known_media = url.is_some_and(|url| media_urls.read().contains(url));
    let verdict = match check_headers(&internal.headers, internal.status.raw_code(), known_media) {
        Some(verdict) => verdict,
        None => {
            let bytes = read_head(internal, done_chan, head).await;
            check_body(&internal.headers, &bytes, &MimeClassifier::default())
        },
    };
    debug!("Opaque Response Blocking: {verdict:?} for {url:?}");
    if verdict == Verdict::AllowMedia {
        if let Some(url) = url {
            media_urls.write().insert(url.clone());
        }
    }
    verdict != Verdict::Block
}

/// Reads body data until [`SNIFF_LEN`] bytes or the end of the body, keeping
/// every message read in `head`.
async fn read_head(
    internal: &Response,
    done_chan: &mut DoneChannel,
    head: &mut VecDeque<Data>,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SNIFF_LEN);
    let Some((_, receiver)) = done_chan else {
        if let ResponseBody::Done(ref body) = *internal.body.lock() {
            bytes.extend_from_slice(&body[..body.len().min(SNIFF_LEN)]);
        }
        return bytes;
    };
    while bytes.len() < SNIFF_LEN {
        let Some(data) = receiver.recv().await else {
            break;
        };
        let more = match &data {
            Data::Payload(payload) => {
                let wanted = (SNIFF_LEN - bytes.len()).min(payload.len());
                bytes.extend_from_slice(&payload[..wanted]);
                true
            },
            Data::ContentLength(_) => true,
            Data::Done | Data::Cancelled | Data::Error(_) => false,
        };
        head.push_back(data);
        if !more {
            break;
        }
    }
    bytes
}

fn is_never_sniffed(mime: &Mime) -> bool {
    let essence = mime.essence_str();
    NEVER_SNIFFED.contains(&essence) ||
        essence.starts_with("application/vnd.openxmlformats-officedocument.")
}

/// Whether a `Content-Range` header says the range starts at the first byte.
fn range_starts_at_zero(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("bytes"))
        .and_then(|range| range.trim_start().split('-').next())
        .and_then(|start| start.trim().parse::<u64>().ok()) ==
        Some(0)
}

fn skip_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b'\t' | b'\n' | 0x0C | b'\r' | b' '))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// Skips a UTF-8 byte order mark and then whitespace.
fn skip_to_content(bytes: &[u8]) -> &[u8] {
    skip_whitespace(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
}

fn looks_like_html(head: &[u8]) -> bool {
    let mut rest = skip_to_content(head);
    while let Some(comment) = rest.strip_prefix(b"<!--") {
        let Some(end) = comment.windows(3).position(|window| window == b"-->") else {
            return false;
        };
        rest = skip_whitespace(&comment[end + 3..]);
    }
    HTML_TAGS.iter().any(|tag| {
        rest.len() > tag.len() &&
            rest[..tag.len()].eq_ignore_ascii_case(tag) &&
            matches!(
                rest[tag.len()],
                b'\t' | b'\n' | 0x0C | b'\r' | b' ' | b'>'
            )
    })
}

fn looks_like_xml(head: &[u8]) -> bool {
    skip_to_content(head).starts_with(b"<?xml")
}

/// Whether the bytes start a JSON object: `{`, a string, then `:`. That is
/// never the start of a script.
fn looks_like_json_object(head: &[u8]) -> bool {
    let Some(rest) = skip_to_content(head).strip_prefix(b"{") else {
        return false;
    };
    let Some(mut rest) = skip_whitespace(rest).strip_prefix(b"\"") else {
        return false;
    };
    loop {
        match rest.split_first() {
            Some((b'\\', tail)) => rest = tail.get(1..).unwrap_or_default(),
            Some((b'"', tail)) => return skip_whitespace(tail).first() == Some(&b':'),
            Some((b'\n' | b'\r', _)) | None => return false,
            Some((_, tail)) => rest = tail,
        }
    }
}

fn has_non_script_prefix(head: &[u8]) -> bool {
    let rest = skip_to_content(head);
    NON_SCRIPT_PREFIXES
        .iter()
        .any(|prefix| rest.starts_with(prefix))
}
