/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fs;
use std::path::{Path, PathBuf};

use http::header::{
    ACCEPT_LANGUAGE, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, COOKIE, EXPIRES,
    HeaderValue, RANGE, VARY,
};
use http::{HeaderMap, StatusCode};
use net::http_cache::{CacheKey, HttpCache, refresh};
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{Referrer, Request, RequestBuilder};
use net_traits::response::{Response, ResponseBody};
use net_traits::{ResourceFetchTiming, ResourceTimingType};
use servo_base::id::TEST_PIPELINE_ID;
use servo_url::ServoUrl;
use tokio::sync::mpsc::unbounded_channel as unbounded;

#[tokio::test]
async fn test_refreshing_resource_sets_done_chan_the_appropriate_value() {
    let response_bodies = vec![
        ResponseBody::Receiving(vec![]),
        ResponseBody::Empty,
        ResponseBody::Done(vec![]),
    ];
    let url = ServoUrl::parse("https://servo.org").unwrap();
    let request = RequestBuilder::new(
        None,
        UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .pipeline_id(Some(TEST_PIPELINE_ID))
    .origin(url.origin())
    .build();
    let timing = ResourceFetchTiming::new(ResourceTimingType::Navigation);
    let mut response = Response::new(url.clone(), timing);
    // Expires header makes the response cacheable.
    response
        .headers
        .insert(EXPIRES, HeaderValue::from_str("-10").unwrap());
    let cache = HttpCache::default();
    for body in response_bodies {
        *response.body.lock() = body.clone();
        // First, store the 'normal' response.
        let mut resource = {
            let guard = cache.get_or_guard(CacheKey::new(&request)).await;
            guard.insert(&request, &response);
            cache.get_or_guard(CacheKey::new(&request)).await
        };
        // Second, mutate the response into a 304 response, and refresh the stored one.
        response.status = StatusCode::NOT_MODIFIED.into();
        let (send, recv) = unbounded();
        let mut done_chan = Some((send, recv));
        let refreshed_response = refresh(
            &request,
            response.clone(),
            &mut done_chan,
            resource.try_as_mut().unwrap(),
        );
        // Ensure a resource was found, and refreshed.
        assert!(refreshed_response.is_some());
        match body {
            ResponseBody::Receiving(_) => assert!(done_chan.is_some()),
            ResponseBody::Empty | ResponseBody::Done(_) => assert!(done_chan.is_none()),
        }
    }
}

#[tokio::test]
async fn test_skip_incomplete_cache_for_range_request_with_no_end_bound() {
    let actual_body_len = 10;
    let incomplete_response_body = &[1, 2, 3, 4, 5];
    let url = ServoUrl::parse("https://servo.org").unwrap();

    let cache = HttpCache::default();
    let mut headers = HeaderMap::new();

    headers.insert(
        RANGE,
        HeaderValue::from_str(&format!("bytes={}-", 0)).unwrap(),
    );
    let request = RequestBuilder::new(
        None,
        UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .pipeline_id(Some(TEST_PIPELINE_ID))
    .origin(url.origin())
    .headers(headers)
    .build();

    // Store incomplete response to http_cache
    let timing = ResourceFetchTiming::new(ResourceTimingType::Navigation);
    let mut initial_incomplete_response = Response::new(url.clone(), timing);
    *initial_incomplete_response.body.lock() =
        ResponseBody::Done(incomplete_response_body.to_vec());
    initial_incomplete_response.headers.insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!(
            "bytes 0-{}/{}",
            actual_body_len - 1,
            actual_body_len
        ))
        .unwrap(),
    );
    initial_incomplete_response.headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&format!("{}", actual_body_len)).unwrap(),
    );
    initial_incomplete_response
        .headers
        .insert(EXPIRES, HeaderValue::from_str("0").unwrap());
    initial_incomplete_response.status = StatusCode::PARTIAL_CONTENT.into();
    cache.store(&request, &initial_incomplete_response).await;

    // Try to construct response from http_cache
    let mut done_chan = None;
    let consecutive_response = cache.construct_response(&request, &mut done_chan).await;
    assert!(
        consecutive_response.is_none(),
        "Should not construct response from incomplete response!"
    );
}

const DISK_CAPACITY: u64 = 1024 * 1024;

fn request_for(url: &ServoUrl) -> Request {
    RequestBuilder::new(
        None,
        UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .pipeline_id(Some(TEST_PIPELINE_ID))
    .origin(url.origin())
    .build()
}

/// A complete response that stays fresh for an hour.
fn fresh_response(url: &ServoUrl, body: &[u8]) -> Response {
    let timing = ResourceFetchTiming::new(ResourceTimingType::Navigation);
    let mut response = Response::new(url.clone(), timing);
    *response.body.lock() = ResponseBody::Done(body.to_vec());
    response
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=3600"));
    response
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

fn disk_cache(directory: &Path) -> HttpCache {
    HttpCache::new(Some((directory.to_owned(), DISK_CAPACITY)))
}

fn files_with_extension(directory: &Path, extension: &str) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|found| found == extension))
        .collect()
}

#[tokio::test]
async fn test_response_survives_on_disk() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    let body = b"body { color: red }";

    disk_cache(directory.path())
        .store(&request, &fresh_response(&url, body))
        .await;

    // A new cache on the same directory is what a restart of the browser gets.
    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    let response = cache
        .construct_response(&request, &mut done_chan)
        .await
        .expect("The response should come back from disk");
    assert_eq!(*response.body.lock(), ResponseBody::Done(body.to_vec()));
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers.get(CONTENT_TYPE).unwrap(), "text/plain");
    assert_eq!(response.headers.get(CACHE_CONTROL).unwrap(), "max-age=3600");
    assert!(done_chan.is_none());
}

#[tokio::test]
async fn test_memory_only_cache_writes_nothing() {
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    let cache = HttpCache::default();
    cache.store(&request, &fresh_response(&url, b"body")).await;

    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request, &mut done_chan)
            .await
            .is_some()
    );
    // A second cache shares nothing with the first one.
    assert!(
        HttpCache::default()
            .construct_response(&request, &mut done_chan)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn test_truncated_body_on_disk_is_not_used() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    disk_cache(directory.path())
        .store(&request, &fresh_response(&url, b"a body of some length"))
        .await;

    let bodies = files_with_extension(directory.path(), "body");
    assert_eq!(bodies.len(), 1);
    fs::write(&bodies[0], b"a body").unwrap();

    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request, &mut done_chan)
            .await
            .is_none(),
        "A truncated body should not be served"
    );
    assert!(files_with_extension(directory.path(), "meta").is_empty());
    assert!(files_with_extension(directory.path(), "body").is_empty());
}

#[tokio::test]
async fn test_corrupt_body_on_disk_is_not_used() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    disk_cache(directory.path())
        .store(&request, &fresh_response(&url, b"0123456789"))
        .await;

    // The same length, other bytes.
    let bodies = files_with_extension(directory.path(), "body");
    fs::write(&bodies[0], b"9876543210").unwrap();

    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request, &mut done_chan)
            .await
            .is_none(),
        "A corrupt body should not be served"
    );
    assert!(files_with_extension(directory.path(), "meta").is_empty());
}

#[tokio::test]
async fn test_corrupt_metadata_on_disk_is_not_used() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    disk_cache(directory.path())
        .store(&request, &fresh_response(&url, b"0123456789"))
        .await;

    let metadata = files_with_extension(directory.path(), "meta");
    assert_eq!(metadata.len(), 1);
    fs::write(&metadata[0], b"{\"version\":1,").unwrap();

    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request, &mut done_chan)
            .await
            .is_none()
    );
    assert!(files_with_extension(directory.path(), "body").is_empty());
}

#[tokio::test]
async fn test_partial_response_is_not_written_to_disk() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/video.webm").unwrap();
    let request = request_for(&url);
    let mut response = fresh_response(&url, b"01234");
    response
        .headers
        .insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-4/10"));
    response.status = StatusCode::PARTIAL_CONTENT.into();
    disk_cache(directory.path()).store(&request, &response).await;

    assert!(files_with_extension(directory.path(), "meta").is_empty());
    assert!(files_with_extension(directory.path(), "body").is_empty());
}

#[tokio::test]
async fn test_newer_response_replaces_the_stored_one() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    {
        let cache = disk_cache(directory.path());
        cache.store(&request, &fresh_response(&url, b"old")).await;
        cache.store(&request, &fresh_response(&url, b"new")).await;
    }
    assert_eq!(files_with_extension(directory.path(), "body").len(), 1);

    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    let response = cache
        .construct_response(&request, &mut done_chan)
        .await
        .unwrap();
    assert_eq!(*response.body.lock(), ResponseBody::Done(b"new".to_vec()));
}

#[tokio::test]
async fn test_refreshed_response_is_written_to_disk_again() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/style.css").unwrap();
    let request = request_for(&url);
    let timing = ResourceFetchTiming::new(ResourceTimingType::Navigation);
    // Stale from the start, so only a validation makes it usable.
    let mut stale = Response::new(url.clone(), timing.clone());
    *stale.body.lock() = ResponseBody::Done(b"body".to_vec());
    stale
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
    let bodies;
    {
        let cache = disk_cache(directory.path());
        cache.store(&request, &stale).await;
        bodies = files_with_extension(directory.path(), "body");
        assert_eq!(bodies.len(), 1);

        let mut not_modified = Response::new(url.clone(), timing);
        not_modified.status = StatusCode::NOT_MODIFIED.into();
        not_modified
            .headers
            .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=3600"));
        {
            let mut resources = cache.get_or_guard(CacheKey::new(&request)).await;
            let mut done_chan = None;
            let refreshed = refresh(
                &request,
                not_modified,
                &mut done_chan,
                resources.try_as_mut().unwrap(),
            )
            .unwrap();
            assert_eq!(refreshed.headers.get(CACHE_CONTROL).unwrap(), "max-age=3600");
        }
        // The fetch that validated the response ends.
        cache.update_awaiting_consumers(&request, &stale).await;
    }
    // The body did not change, so it is the one written the first time.
    assert_eq!(bodies, files_with_extension(directory.path(), "body"));

    let cache = disk_cache(directory.path());
    let mut done_chan = None;
    let response = cache
        .construct_response(&request, &mut done_chan)
        .await
        .unwrap();
    assert_eq!(response.headers.get(CACHE_CONTROL).unwrap(), "max-age=3600");
    assert_eq!(*response.body.lock(), ResponseBody::Done(b"body".to_vec()));
}

fn request_with_language(url: &ServoUrl, language: &'static str) -> Request {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static(language));
    headers.insert(COOKIE, HeaderValue::from_static("session=secret"));
    RequestBuilder::new(
        None,
        UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .pipeline_id(Some(TEST_PIPELINE_ID))
    .origin(url.origin())
    .headers(headers)
    .build()
}

fn response_varying_on_language(url: &ServoUrl, body: &[u8]) -> Response {
    let mut response = fresh_response(url, body);
    response
        .headers
        .insert(VARY, HeaderValue::from_static("Accept-Language"));
    response
}

#[tokio::test]
async fn test_variants_survive_on_disk_without_other_request_headers() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/").unwrap();
    let catalan = request_with_language(&url, "ca");
    let english = request_with_language(&url, "en");
    {
        let cache = disk_cache(directory.path());
        cache
            .store(&catalan, &response_varying_on_language(&url, b"bon dia"))
            .await;
        cache
            .store(&english, &response_varying_on_language(&url, b"good morning"))
            .await;
    }

    let metadata = files_with_extension(directory.path(), "meta");
    assert_eq!(metadata.len(), 1);
    let metadata = fs::read_to_string(&metadata[0]).unwrap();
    assert!(metadata.contains(r#"["accept-language","ca"]"#), "{metadata}");
    assert!(!metadata.contains("secret"), "{metadata}");

    let cache = disk_cache(directory.path());
    for (request, body) in [(&catalan, &b"bon dia"[..]), (&english, &b"good morning"[..])] {
        let mut done_chan = None;
        let response = cache
            .construct_response(request, &mut done_chan)
            .await
            .unwrap();
        assert_eq!(*response.body.lock(), ResponseBody::Done(body.to_vec()));
    }
    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request_with_language(&url, "fr"), &mut done_chan)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn test_variants_of_one_url_are_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://servo.org/").unwrap();
    let languages = ["ca", "en", "fr", "de", "it", "pt"];
    let cache = disk_cache(directory.path());
    for language in languages {
        cache
            .store(
                &request_with_language(&url, language),
                &response_varying_on_language(&url, language.as_bytes()),
            )
            .await;
    }
    assert_eq!(files_with_extension(directory.path(), "body").len(), 4);

    // The newest ones are the ones kept.
    let mut done_chan = None;
    assert!(
        cache
            .construct_response(&request_with_language(&url, "pt"), &mut done_chan)
            .await
            .is_some()
    );
    assert!(
        cache
            .construct_response(&request_with_language(&url, "ca"), &mut done_chan)
            .await
            .is_none()
    );
}
