/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use http::header::{CONTENT_RANGE, CONTENT_TYPE, HeaderName};
use http::{HeaderMap, HeaderValue};
use net::fetch::orb::{MediaUrls, Verdict, check_body, check_headers};
use net_traits::mime_classifier::MimeClassifier;
use servo_url::ServoUrl;

const PNG: &[u8] = b"\x89PNG\r\n\x1A\n\0\0\0\rIHDR";
const WEBM: &[u8] = b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01";

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
            HeaderValue::from_str(value).expect("valid header value"),
        );
    }
    headers
}

fn typed(content_type: &str) -> HeaderMap {
    headers(&[(CONTENT_TYPE.as_str(), content_type)])
}

fn body_verdict(headers: &HeaderMap, body: &[u8]) -> Verdict {
    check_body(headers, body, &MimeClassifier::default())
}

#[test]
fn test_scripts_stylesheets_and_svg_pass_on_their_type() {
    for content_type in [
        "text/javascript",
        "application/javascript; charset=utf-8",
        "text/css",
        "image/svg+xml",
    ] {
        assert_eq!(
            check_headers(&typed(content_type), 200, false),
            Some(Verdict::Allow),
            "{content_type}"
        );
    }
}

#[test]
fn test_never_sniffed_types_are_blocked_on_their_type() {
    for content_type in [
        "application/pdf",
        "application/zip",
        "text/csv",
        "text/event-stream",
        "application/x-protobuf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ] {
        assert_eq!(
            check_headers(&typed(content_type), 200, false),
            Some(Verdict::Block),
            "{content_type}"
        );
    }
}

#[test]
fn test_other_types_need_the_body() {
    for content_type in ["text/html", "application/json", "image/png", "video/mp4"] {
        assert_eq!(check_headers(&typed(content_type), 200, false), None);
    }
    assert_eq!(check_headers(&HeaderMap::new(), 200, false), None);
}

#[test]
fn test_a_range_from_the_middle_passes_only_for_known_media() {
    let middle = headers(&[
        (CONTENT_TYPE.as_str(), "video/mp4"),
        (CONTENT_RANGE.as_str(), "bytes 1000-1999/5000"),
    ]);
    assert_eq!(check_headers(&middle, 206, false), Some(Verdict::Block));
    assert_eq!(check_headers(&middle, 206, true), Some(Verdict::Allow));

    let missing = typed("video/mp4");
    assert_eq!(check_headers(&missing, 206, false), Some(Verdict::Block));

    let start = headers(&[
        (CONTENT_TYPE.as_str(), "video/mp4"),
        (CONTENT_RANGE.as_str(), "bytes 0-999/5000"),
    ]);
    assert_eq!(check_headers(&start, 206, false), None);
}

#[test]
fn test_images_and_media_pass_whatever_their_type() {
    assert_eq!(body_verdict(&typed("text/html"), PNG), Verdict::Allow);
    assert_eq!(
        body_verdict(&typed("application/octet-stream"), WEBM),
        Verdict::AllowMedia
    );
}

#[test]
fn test_nosniff_documents_are_blocked() {
    let nosniff = headers(&[
        (CONTENT_TYPE.as_str(), "application/json"),
        ("x-content-type-options", "nosniff"),
    ]);
    assert_eq!(body_verdict(&nosniff, b"var x = 1;"), Verdict::Block);
}

#[test]
fn test_html_is_blocked_when_declared_and_confirmed() {
    let html = typed("text/html");
    assert_eq!(
        body_verdict(&html, b"\xEF\xBB\xBF <!-- a -->\n<HTML lang=en>"),
        Verdict::Block
    );
    assert_eq!(body_verdict(&html, b"<!doctype html>"), Verdict::Block);
    assert_eq!(body_verdict(&html, b"<p>hello"), Verdict::Block);
    // A script served with the wrong type still runs.
    assert_eq!(body_verdict(&html, b"var page = 1;"), Verdict::Allow);
    assert_eq!(body_verdict(&html, b"<pre>"), Verdict::Allow);
    // A comment that does not end within the bytes looked at decides nothing.
    assert_eq!(body_verdict(&html, b"<!-- <html>"), Verdict::Allow);
}

#[test]
fn test_html_bytes_with_another_type_pass() {
    assert_eq!(
        body_verdict(&typed("application/octet-stream"), b"<html>"),
        Verdict::Allow
    );
    assert_eq!(body_verdict(&HeaderMap::new(), b"<html>"), Verdict::Allow);
}

#[test]
fn test_xml_is_blocked_when_declared_and_confirmed() {
    assert_eq!(
        body_verdict(&typed("application/xml"), b"<?xml version=\"1.0\"?>"),
        Verdict::Block
    );
    assert_eq!(
        body_verdict(&typed("text/plain"), b"  <?xml version=\"1.0\"?>"),
        Verdict::Block
    );
}

#[test]
fn test_json_objects_are_blocked_whatever_their_type() {
    for headers in [
        typed("application/json"),
        typed("text/plain"),
        typed("application/octet-stream"),
        HeaderMap::new(),
    ] {
        assert_eq!(
            body_verdict(&headers, b" {\n  \"key\\\"s\" : 1}"),
            Verdict::Block
        );
    }
    // A block statement is not a JSON object.
    assert_eq!(body_verdict(&HeaderMap::new(), b"{ a(); }"), Verdict::Allow);
    // A string that does not end within the bytes looked at decides nothing.
    assert_eq!(body_verdict(&HeaderMap::new(), b"{\"key"), Verdict::Allow);
}

#[test]
fn test_prefixes_against_running_as_a_script_are_blocked() {
    for body in [
        &b")]}'\n[1]"[..],
        b"{}&&{}",
        b"{} &&{}",
        b"for(;;);[1]",
        b"for (;;);[1]",
        b"while(1);[1]",
        b"while (1);[1]",
    ] {
        assert_eq!(body_verdict(&typed("text/javascript"), body), Verdict::Block);
    }
}

#[test]
fn test_plain_text_that_is_a_script_passes() {
    assert_eq!(
        body_verdict(&typed("text/plain"), b"window.x = [1, 2];"),
        Verdict::Allow
    );
    assert_eq!(body_verdict(&typed("text/plain"), b""), Verdict::Allow);
}

#[test]
fn test_media_urls_forget_the_oldest() {
    let mut urls = MediaUrls::default();
    let first = ServoUrl::parse("https://a.example/0").expect("valid URL");
    urls.insert(first.clone());
    urls.insert(first.clone());
    assert!(urls.contains(&first));
    for index in 1..=256 {
        urls.insert(ServoUrl::parse(&format!("https://a.example/{index}")).expect("valid URL"));
    }
    assert!(!urls.contains(&first));
    assert!(urls.contains(&ServoUrl::parse("https://a.example/256").expect("valid URL")));
}
