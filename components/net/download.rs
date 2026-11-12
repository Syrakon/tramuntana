/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Navigations whose response is a file to save. The decision is taken when the headers
//! arrive, so the body never builds up in memory or in the HTTP cache; the body then goes
//! to the embedder, and the script thread only learns that the navigation ended.

use std::sync::Arc;

use bytes::Bytes;
use content_security_policy as csp;
use crossbeam_channel::{Sender, unbounded};
use embedder_traits::{Download, DownloadEvent, GenericEmbedderProxy};
use http::HeaderMap;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use mime::Mime;
use net_traits::FetchTaskTarget;
use net_traits::http_status::HttpStatus;
use net_traits::mime_classifier::MimeClassifier;
use net_traits::request::{Request, RequestMode};
use net_traits::response::Response;

use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::CancellationListener;

/// Whether the response to `request` is saved to a file instead of rendered: a navigation
/// whose server says so with `Content-Disposition: attachment`, or that declares a type
/// no document can be made of. A response with no type is left to sniffing, as are the
/// types that ask for it, and `multipart/*` stays a document so that streams do not turn
/// into downloads that never end.
pub(crate) fn is_download(request: &Request, status: &HttpStatus, headers: &HeaderMap) -> bool {
    request.mode == RequestMode::Navigate && response_is_file(status, headers)
}

fn response_is_file(status: &HttpStatus, headers: &HeaderMap) -> bool {
    if !status.in_range(200..=299) || status.in_range(204..=205) {
        return false;
    }
    if disposition_is_attachment(headers) {
        return true;
    }
    let Some(mime) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Mime>().ok())
    else {
        return false;
    };
    let asks_for_sniffing = matches!(
        mime.essence_str(),
        "unknown/unknown" | "application/unknown" | "*/*" | "application/x-unknown-content-type"
    );
    if asks_for_sniffing || mime.type_() == mime::MULTIPART {
        return false;
    }
    MimeClassifier::get_media_type(&mime).is_none()
}

/// Read from the raw bytes: servers often put a Latin-1 `filename` in the same header,
/// and that must not hide the `attachment` in front of it.
fn disposition_is_attachment(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.as_bytes().split(|byte| *byte == b';').next())
        .is_some_and(|kind| kind.trim_ascii().eq_ignore_ascii_case(b"attachment"))
}

/// The file name in `Content-Disposition`: `filename*` (RFC 6266 and 8187) wins over
/// `filename`. It is returned as the server sent it, path separators included; making
/// it safe to use on disk is up to whoever saves the file.
pub(crate) fn suggested_file_name(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_DISPOSITION)?;
    // UTF-8 when the bytes are; otherwise the Latin-1 that HTTP headers were defined in.
    let value = String::from_utf8(value.as_bytes().to_vec()).unwrap_or_else(|_| {
        value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect()
    });
    let mut plain = None;
    let mut extended = None;
    for parameter in split_parameters(&value).into_iter().skip(1) {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("filename*") {
            extended = decode_extended_value(value);
        } else if name.eq_ignore_ascii_case("filename") {
            plain = Some(unquote(value));
        }
    }
    extended.or(plain).filter(|name| !name.is_empty())
}

/// Split on the `;` that are not inside a quoted string.
fn split_parameters(value: &str) -> Vec<&str> {
    let mut parts = vec![];
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&value[start..index]);
                start = index + 1;
            },
            _ => {},
        }
    }
    parts.push(&value[start..]);
    parts
}

fn unquote(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .map(|rest| rest.strip_suffix('"').unwrap_or(rest))
    else {
        return value.to_owned();
    };
    let mut unquoted = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => unquoted.extend(characters.next()),
            _ => unquoted.push(character),
        }
    }
    unquoted
}

/// `charset'language'percent-encoded`. UTF-8 is decoded as such; ISO-8859-1 maps each
/// byte to the character of the same number; any other charset is not understood.
fn decode_extended_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?;
    let encoded = parts.next()?;
    let bytes = percent_decode(encoded)?;
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.into_iter().map(char::from).collect())
    } else {
        None
    }
}

fn percent_decode(encoded: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut input = encoded.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = char::from(input.next()?).to_digit(16)?;
            let low = char::from(input.next()?).to_digit(16)?;
            bytes.push(u8::try_from(high * 16 + low).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(bytes)
}

/// Stands between a fetch and the target it reports to. While the response is not a
/// download it passes everything through. When it is, the target still hears the headers
/// (whose metadata says it is a download) and the end, but the body goes to the embedder.
pub(crate) struct DownloadDiverter<Target> {
    inner: Target,
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
    cancellation_listener: Arc<CancellationListener>,
    events: Option<Sender<DownloadEvent>>,
}

impl<Target> DownloadDiverter<Target> {
    pub(crate) fn new(
        inner: Target,
        embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
        cancellation_listener: Arc<CancellationListener>,
    ) -> Self {
        Self {
            inner,
            embedder_proxy,
            cancellation_listener,
            events: None,
        }
    }

    fn start(&mut self, request: &Request, response: &Response) {
        let Some(webview_id) = request.target_webview_id else {
            return;
        };
        let actual = response.actual_response();
        let Some(url) = actual.url().map(|url| url.as_url().clone()) else {
            return;
        };
        let headers = &actual.headers;
        let mime_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let expected_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());
        let (sender, receiver) = unbounded();
        let cancellation_listener = self.cancellation_listener.clone();
        let download = Download::new(
            url,
            suggested_file_name(headers),
            mime_type,
            expected_length,
            receiver,
            Box::new(move || cancellation_listener.cancel()),
        );
        self.events = Some(sender);
        self.embedder_proxy
            .send(NetToEmbedderMsg::Download(webview_id, download));
    }
}

impl<Target: FetchTaskTarget> FetchTaskTarget for DownloadDiverter<Target> {
    fn process_request_body(&mut self, request: &Request) {
        self.inner.process_request_body(request);
    }

    fn process_response(&mut self, request: &Request, response: &Response) {
        if response.actual_response().download {
            self.start(request, response);
        }
        self.inner.process_response(request, response);
    }

    fn process_response_chunk(&mut self, request: &Request, chunk: Bytes) {
        match &self.events {
            Some(events) => {
                if events.send(DownloadEvent::Data(chunk.to_vec())).is_err() {
                    // Nobody is reading any more: stop the transfer.
                    self.cancellation_listener.cancel();
                }
            },
            None => self.inner.process_response_chunk(request, chunk),
        }
    }

    fn process_response_eof(&mut self, request: &Request, response: &Response) {
        if let Some(events) = self.events.take() {
            let event = if response
                .actual_response()
                .aborted
                .load(std::sync::atomic::Ordering::Acquire)
            {
                DownloadEvent::Failed("cancelled or cut short".to_owned())
            } else if let Some(error) = response.get_network_error() {
                DownloadEvent::Failed(format!("{error:?}"))
            } else {
                DownloadEvent::Finished
            };
            // The embedder may have dropped the download already; then there is nobody to tell.
            events.send(event).ok();
        }
        self.inner.process_response_eof(request, response);
    }

    fn process_csp_violations(&mut self, request: &Request, violations: Vec<csp::Violation>) {
        self.inner.process_csp_violations(request, violations);
    }

    fn process_response_length_hint(&mut self, request: &Request, length: usize) {
        if self.events.is_none() {
            self.inner.process_response_length_hint(request, length);
        }
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn with_disposition(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn file_name_from_plain_and_extended_parameters() {
        let name = |value| suggested_file_name(&with_disposition(value));
        assert_eq!(name("attachment; filename=a.zip").as_deref(), Some("a.zip"));
        assert_eq!(
            name("attachment; filename=\"a; b.zip\"").as_deref(),
            Some("a; b.zip")
        );
        assert_eq!(
            name("attachment; filename=\"q\\\"t.txt\"").as_deref(),
            Some("q\"t.txt")
        );
        assert_eq!(
            name("attachment; filename=\"plain.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt")
                .as_deref(),
            Some("€ rates.txt")
        );
        assert_eq!(
            name("attachment; filename*=iso-8859-1'en'%A3%20rates").as_deref(),
            Some("£ rates")
        );
        assert_eq!(name("attachment").as_deref(), None);
        assert_eq!(name("attachment; filename=\"\"").as_deref(), None);
        // A broken extended value falls back to the plain one.
        assert_eq!(
            name("attachment; filename=x.bin; filename*=UTF-8''%zz").as_deref(),
            Some("x.bin")
        );
    }

    fn response(status: u16, content_type: Option<&str>, disposition: Option<&str>) -> bool {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
        }
        if let Some(value) = disposition {
            headers.insert(CONTENT_DISPOSITION, HeaderValue::from_str(value).unwrap());
        }
        response_is_file(&HttpStatus::new_raw(status, vec![]), &headers)
    }

    #[test]
    fn which_responses_are_files() {
        assert!(response(200, Some("application/zip"), None));
        assert!(response(200, Some("application/octet-stream"), None));
        assert!(response(200, Some("application/pdf"), None));
        assert!(response(
            200,
            Some("text/html"),
            Some("attachment; filename=a.html")
        ));
        assert!(!response(200, Some("text/html; charset=utf-8"), None));
        assert!(!response(200, Some("text/plain"), None));
        assert!(!response(200, Some("image/png"), None));
        assert!(!response(200, Some("application/json"), None));
        assert!(!response(200, Some("text/html"), Some("inline")));
        // Left to sniffing, or kept as documents.
        assert!(!response(200, None, None));
        assert!(!response(200, Some("unknown/unknown"), None));
        assert!(!response(
            200,
            Some("multipart/x-mixed-replace; boundary=x"),
            None
        ));
        // Only successful responses with a body.
        assert!(!response(404, Some("application/zip"), None));
        assert!(!response(204, None, Some("attachment")));
        assert!(!response(206, Some("text/html"), None));
    }

    #[test]
    fn attachment_is_case_insensitive_and_inline_is_not() {
        assert!(disposition_is_attachment(&with_disposition(
            "Attachment; filename=a"
        )));
        assert!(!disposition_is_attachment(&with_disposition(
            "inline; filename=a"
        )));
        assert!(!disposition_is_attachment(&HeaderMap::new()));
    }

    #[test]
    fn a_latin1_file_name_does_not_hide_the_attachment() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_bytes(b"attachment; filename=\"informe \xf1.html\"").unwrap(),
        );
        assert!(disposition_is_attachment(&headers));
        assert_eq!(
            suggested_file_name(&headers).as_deref(),
            Some("informe ñ.html")
        );
    }
}
