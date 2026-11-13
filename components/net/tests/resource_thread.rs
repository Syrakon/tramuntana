/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::net::IpAddr;

use cookie::Cookie;
use hyper_serde::Serde;
use net::connector::CACertificates;
use net::protocols::ProtocolRegistry;
use net::resource_thread::new_core_resource_thread;
use net::test::parse_hostsfile;
use net_traits::{CookieSource, CoreResourceMsg, CoreResourceThread};
use profile_traits::mem::ProfilerChan as MemProfilerChan;
use profile_traits::time::ProfilerChan;
use servo_base::generic_channel;
use servo_url::ServoUrl;

use crate::create_generic_embedder_proxy;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn test_exit() {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    let (resource_thread, _private_resource_thread) = new_core_resource_thread(
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        None,
        None,
        CACertificates::Default,
        false, /* ignore_certificate_errors */
        std::sync::Arc::new(ProtocolRegistry::default()),
        None,
    );
    resource_thread.send(CoreResourceMsg::Exit(sender)).unwrap();
    receiver.recv().unwrap();
    // Workaround for https://github.com/servo/servo/issues/32912
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(100));
}

fn set_cookie(resource_thread: &CoreResourceThread, url: &ServoUrl, cookie: &str) {
    let (sender, receiver) = generic_channel::channel().unwrap();
    resource_thread
        .send(CoreResourceMsg::SetCookieForUrl(
            url.clone(),
            Serde(Cookie::parse(cookie.to_owned()).unwrap()),
            CookieSource::HTTP,
            Some(sender),
        ))
        .unwrap();
    receiver.recv().unwrap();
}

fn cookie_string(resource_thread: &CoreResourceThread, url: &ServoUrl) -> Option<String> {
    let (sender, receiver) = generic_channel::channel().unwrap();
    resource_thread
        .send(CoreResourceMsg::GetCookieStringForUrl(
            url.clone(),
            sender,
            CookieSource::HTTP,
            None,
        ))
        .unwrap();
    receiver.recv().unwrap()
}

fn new_temporary_session(
    resource_thread: &CoreResourceThread,
    accept_cookies: bool,
) -> CoreResourceThread {
    new_session_in(resource_thread, accept_cookies, None)
}

fn new_session_in(
    resource_thread: &CoreResourceThread,
    accept_cookies: bool,
    dir: Option<&std::path::Path>,
) -> CoreResourceThread {
    let (sender, receiver) = generic_channel::channel().unwrap();
    resource_thread
        .send(CoreResourceMsg::NewTemporarySession(
            accept_cookies,
            dir.map(std::path::Path::to_owned),
            sender,
        ))
        .unwrap();
    receiver.recv().unwrap().unwrap()
}

fn public_resource_thread() -> CoreResourceThread {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (public, _private) = new_core_resource_thread(
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        None,
        None,
        CACertificates::Default,
        false, /* ignore_certificate_errors */
        std::sync::Arc::new(ProtocolRegistry::default()),
        None,
    );
    public
}

fn exit(resource_thread: &CoreResourceThread) {
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    resource_thread.send(CoreResourceMsg::Exit(sender)).unwrap();
    receiver.recv().unwrap();
}

/// A session with a directory finds there the cookies it kept, whether it ended
/// on its own or with the resource thread, and shares them with no other session.
#[test]
fn test_a_session_with_a_directory_keeps_its_cookies() {
    let dir = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://example.org/").unwrap();
    let lasting = "kept=1; Max-Age=3600";

    let public = public_resource_thread();
    let session = new_session_in(&public, true, Some(dir.path()));
    set_cookie(&session, &url, lasting);
    set_cookie(&session, &url, "for_now=1");
    assert_eq!(cookie_string(&public, &url), None);

    // The session ends when its last sender goes, and writes its jar on the way.
    drop(session);
    let jar = dir.path().join("cookie_jar.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(&jar).is_ok_and(|text| text.contains("kept")) {
        assert!(std::time::Instant::now() < deadline, "the jar was never written");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Started again on the same directory, it has what lasts and not the
    // cookies of the session before.
    let again = new_session_in(&public, true, Some(dir.path()));
    assert_eq!(cookie_string(&again, &url), Some("kept=1".to_owned()));
    set_cookie(&again, &url, "added=1; Max-Age=3600");
    // `Exit` writes the session's jar too.
    exit(&public);

    let public = public_resource_thread();
    let after_exit = new_session_in(&public, true, Some(dir.path()));
    let cookies = cookie_string(&after_exit, &url).unwrap_or_default();
    assert!(cookies.contains("kept=1") && cookies.contains("added=1"), "{cookies}");
    drop(after_exit);

    // A session that refuses cookies neither takes them from the directory nor
    // leaves its empty jar there.
    let refusing = new_session_in(&public, false, Some(dir.path()));
    assert_eq!(cookie_string(&refusing, &url), None);
    drop(refusing);
    exit(&public);
    let public = public_resource_thread();
    let last = new_session_in(&public, true, Some(dir.path()));
    assert!(cookie_string(&last, &url).is_some_and(|cookies| cookies.contains("kept=1")));
    exit(&public);
}

#[test]
fn test_temporary_sessions_have_their_own_cookies() {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (public, private) = new_core_resource_thread(
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        None,
        None,
        CACertificates::Default,
        false, /* ignore_certificate_errors */
        std::sync::Arc::new(ProtocolRegistry::default()),
        None,
    );
    let url = ServoUrl::parse("https://example.org/").unwrap();

    let keeping = new_temporary_session(&public, true);
    let other = new_temporary_session(&public, true);
    let rejecting = new_temporary_session(&public, false);

    set_cookie(&keeping, &url, "kept=1");
    set_cookie(&rejecting, &url, "refused=1");
    set_cookie(&public, &url, "public=1");

    assert_eq!(cookie_string(&keeping, &url), Some("kept=1".to_owned()));
    assert_eq!(cookie_string(&other, &url), None);
    assert_eq!(cookie_string(&rejecting, &url), None);
    assert_eq!(cookie_string(&private, &url), None);
    assert_eq!(cookie_string(&public, &url), Some("public=1".to_owned()));

    // Ending a session leaves the resource thread serving the others.
    drop(keeping);
    assert_eq!(cookie_string(&public, &url), Some("public=1".to_owned()));
    assert_eq!(cookie_string(&other, &url), None);

    let (sender, receiver) = generic_channel::oneshot().unwrap();
    public.send(CoreResourceMsg::Exit(sender)).unwrap();
    receiver.recv().unwrap();
}

#[test]
fn test_parse_hostsfile() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com\n127.0.0.2 servo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_malformed_hostsfile() {
    let mock_hosts_file_content =
        "malformed file\n127.0.0.1 foo.bar.com\nservo.test.server 127.0.0.1";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_line_comment() {
    let mock_hosts_file_content =
        "# this is a line comment\n127.0.0.1 foo.bar.com\n# anothercomment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_comment() {
    let mock_hosts_file_content =
        "127.0.0.1 foo.bar.com # line ending comment\n127.0.0.2 servo.test.server #comment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_2_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com baz.bar.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("baz.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_4_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 moz.foo.com moz.bar.com moz.baz.com moz.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(4, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.foo.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.baz.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.moz.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_tabs_instead_spaces() {
    let mock_hosts_file_content = "127.0.0.1\tfoo.bar.com\n127.0.0.2\tservo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_valid_ipv4_addresses() {
    let mock_hosts_file_content =
        "255.255.255.255 foo.bar.com\n169.0.1.201 servo.test.server\n192.168.5.0 servo.foo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv4_addresses() {
    let mock_hosts_file_content = "256.255.255.255 foo.bar.com\n169.0.1000.201 servo.test.server \
                                   \n192.168.5.500 servo.foo.com\n192.abc.100.2 test.servo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_valid_ipv6_addresses() {
    let mock_hosts_file_content = "2001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n\
                                   2001:db8::ff00:42:8329 foo.moz.com moz.moz.com\n\
                                   0000:0000:0000:0000:0000:0000:0000:0001 bar.moz.com\n\
                                   ::1 foo.bar.baz baz.foo.com\n\
                                   2001:0DB8:85A3:0042:1000:8A2E:0370:7334 baz.bar.moz\n\
                                   :: unspecified.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(9, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv6_addresses() {
    let mock_hosts_file_content = "12001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:zdb8:0:0:0:gg00:42:t329 moz.foo.com\n\
                                   2002:0DB8:85A3:0042:1000:8A2E:0370:7334/1289 baz3.bar.moz";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_whitespace() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com \n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n \
                                   127.0.0.2 servo.test.server ";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("2001:db8:0:0:0:ff00:42:8329"),
        *hosts_table.get("moz.foo.com").unwrap()
    );
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}
