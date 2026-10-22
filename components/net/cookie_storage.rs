/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implementation of cookie storage as specified in
//! <http://tools.ietf.org/html/rfc6265>

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cookie::Cookie;
use itertools::Itertools;
use log::info;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::cookie_partition::{CookieContext, CookiePartition, top_level_site};
use net_traits::pub_domains::{is_same_site, reg_suffix};
use net_traits::request::Request;
use net_traits::{CookieSource, SiteDescriptor};
use serde::{Deserialize, Serialize};
use servo_url::ServoUrl;

use crate::cookie::ServoCookie;

#[derive(Clone, Debug, Deserialize, Serialize, MallocSizeOf)]
pub struct CookieStorage {
    version: u32,
    cookies_map: HashMap<String, Vec<ServoCookie>>,
    max_per_host: usize,
    /// The sites the user let use their first-party cookies where another site embeds them,
    /// through `document.requestStorageAccess()`.
    #[serde(default)]
    storage_access: Vec<StorageAccessGrant>,
    /// Whether this storage refuses every cookie it is given, so that it
    /// stays empty. It is a property of the session, not of what is stored.
    #[serde(skip)]
    rejects_all: bool,
}

/// How long the user's answer to a site asking for its cookies stays valid, as in Firefox.
const STORAGE_ACCESS_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A site that may use its first-party cookies in its documents embedded by another site,
/// both named as [`top_level_site`] gives them.
#[derive(Clone, Debug, Deserialize, Serialize, MallocSizeOf)]
struct StorageAccessGrant {
    top_level_site: String,
    embedded_site: String,
    /// When the grant ends, in seconds since the Unix epoch.
    expires: u64,
}

impl StorageAccessGrant {
    fn has_expired(&self) -> bool {
        seconds_since_epoch(SystemTime::now()) >= self.expires
    }

    /// Whether either site is one of `hosts`, registered domains with no scheme.
    fn names_any_of(&self, hosts: &[String]) -> bool {
        [&self.top_level_site, &self.embedded_site].into_iter().any(|site| {
            site.split_once("://")
                .is_some_and(|(_, host)| hosts.iter().any(|candidate| candidate == host))
        })
    }
}

fn seconds_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum RemoveCookieError {
    Overlapping,
    NonHTTP,
}

impl CookieStorage {
    pub fn new(max_cookies: usize) -> CookieStorage {
        CookieStorage {
            version: 1,
            cookies_map: HashMap::new(),
            max_per_host: max_cookies,
            storage_access: Vec::new(),
            rejects_all: false,
        }
    }

    /// A storage that keeps no cookie at all, whatever its source.
    pub fn rejecting_all() -> CookieStorage {
        CookieStorage {
            rejects_all: true,
            ..CookieStorage::new(0)
        }
    }

    // http://tools.ietf.org/html/rfc6265#section-5.3
    pub fn remove(
        &mut self,
        cookie: &ServoCookie,
        url: &ServoUrl,
        source: CookieSource,
    ) -> Result<Option<ServoCookie>, RemoveCookieError> {
        let domain = reg_host(cookie.cookie.domain().as_ref().unwrap_or(&""));
        let cookies = self.cookies_map.entry(domain).or_default();

        // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt Step 2
        if !cookie.cookie.secure().unwrap_or(false) && !url.is_secure_scheme() {
            let new_domain = cookie.cookie.domain().as_ref().unwrap().to_owned();
            let new_path = cookie.cookie.path().as_ref().unwrap().to_owned();

            let any_overlapping = cookies.iter().any(|c| {
                if c.top_level_site != cookie.top_level_site {
                    return false;
                }
                let existing_domain = c.cookie.domain().as_ref().unwrap().to_owned();
                let existing_path = c.cookie.path().as_ref().unwrap().to_owned();

                c.cookie.name() == cookie.cookie.name() &&
                    c.cookie.secure().unwrap_or(false) &&
                    (ServoCookie::domain_match(new_domain, existing_domain) ||
                        ServoCookie::domain_match(existing_domain, new_domain)) &&
                    ServoCookie::path_match(new_path, existing_path)
            });

            if any_overlapping {
                return Err(RemoveCookieError::Overlapping);
            }
        }

        // Step 11.1
        let position = cookies.iter().position(|c| {
            c.top_level_site == cookie.top_level_site &&
                c.cookie.domain() == cookie.cookie.domain() &&
                c.cookie.path() == cookie.cookie.path() &&
                c.cookie.name() == cookie.cookie.name()
        });

        if let Some(ind) = position {
            // Step 11.4
            let c = cookies.remove(ind);

            // http://tools.ietf.org/html/rfc6265#section-5.3 step 11.2
            if c.cookie.http_only().unwrap_or(false) && source == CookieSource::NonHTTP {
                // Undo the removal.
                cookies.push(c);
                Err(RemoveCookieError::NonHTTP)
            } else {
                Ok(Some(c))
            }
        } else {
            Ok(None)
        }
    }

    pub fn delete_cookies_for_sites(&mut self, sites: &Vec<String>) {
        // Note: We assume the number of sites is smaller than the number of
        // entries in the cookies map. If this assumption stops holding in
        // practice, this implementation can be revised to use `retain`
        // together with a temporary `HashSet` of sites.
        for site in sites {
            // TODO: We currently mark cookies as expired instead of removing
            // them immediately (same behavior as in the functions below).
            // This is safe because higher-level cookie accessors always call
            // `remove_expired_cookies_for_url` / `remove_all_expired_cookies`.
            // Consider whether we should instead delete the entries directly.
            if let Some(cookies) = self.cookies_map.get_mut(site) {
                for cookie in cookies.iter_mut() {
                    cookie.set_expiry_time_in_past();
                }
            }
        }
        // The cookies other sites have when embedded by these are theirs too.
        self.cookies_map
            .values_mut()
            .flat_map(|cookies| cookies.iter_mut())
            .filter(|cookie| {
                cookie
                    .top_level_site
                    .as_deref()
                    .and_then(|top_level_site| top_level_site.split_once("://"))
                    .is_some_and(|(_, host)| sites.iter().any(|site| site == host))
            })
            .for_each(|cookie| cookie.set_expiry_time_in_past());
        self.storage_access.retain(|grant| !grant.names_any_of(sites));
    }

    pub fn clear_session_cookies(&mut self) {
        self.cookies_map
            .values_mut()
            .flat_map(|cookies| cookies.iter_mut())
            .filter(|cookie| !cookie.persistent)
            .for_each(|cookie| cookie.set_expiry_time_in_past());
    }

    pub fn clear_storage(&mut self, url: Option<&ServoUrl>) {
        if let Some(url) = url {
            let domain = reg_host(url.host_str().unwrap_or(""));
            if let Some(cookies) = self.cookies_map.get_mut(&domain) {
                for cookie in cookies.iter_mut() {
                    cookie.set_expiry_time_in_past();
                }
            }
        } else {
            self.cookies_map.clear();
            self.storage_access.clear();
        }
    }

    /// The cookies for `url` when used from `context`: those of [`CookiePartition::for_url`],
    /// unless a document of the site of `url` asks for them where the user let that site use
    /// its first-party cookies.
    pub fn partition_for(
        &self,
        url: &ServoUrl,
        context: Option<&CookieContext>,
    ) -> CookiePartition {
        let partition = CookiePartition::for_url(url, context);
        let (CookiePartition::Embedded(top_level), Some(context)) = (&partition, context) else {
            return partition;
        };
        if !is_same_site(&url.origin(), &context.origin) {
            return partition;
        }
        let Some(embedded) = top_level_site(&context.origin) else {
            return partition;
        };
        let granted = self.storage_access.iter().any(|grant| {
            grant.top_level_site == *top_level &&
                grant.embedded_site == embedded &&
                !grant.has_expired()
        });
        if granted {
            CookiePartition::FirstParty
        } else {
            partition
        }
    }

    /// [`Self::partition_for`] a request to `url`. The client of a navigation request is the
    /// document that embeds the frame, but the document it loads is of the site of `url`, so
    /// a grant that site has applies to it.
    pub fn partition_for_request(&self, url: &ServoUrl, request: &Request) -> CookiePartition {
        let Some(context) = request.cookie_context() else {
            return self.partition_for(url, None);
        };
        if !request.is_navigation_request() {
            return self.partition_for(url, Some(context));
        }
        let loaded = CookieContext {
            origin: url.origin(),
            ..context.clone()
        };
        self.partition_for(url, Some(&loaded))
    }

    /// Let the documents of the site of `context` use their first-party cookies where its
    /// top-level site embeds them, from now on and for [`STORAGE_ACCESS_LIFETIME`]. Nothing
    /// changes for a context that has them already.
    pub fn grant_storage_access(&mut self, context: &CookieContext) {
        let first_party = !context.has_cross_site_ancestor &&
            is_same_site(&context.origin, &context.top_level_origin);
        if first_party {
            return;
        }
        let (Some(top_level_site_name), Some(embedded_site)) = (
            top_level_site(&context.top_level_origin),
            top_level_site(&context.origin),
        ) else {
            return;
        };
        let expires = seconds_since_epoch(SystemTime::now() + STORAGE_ACCESS_LIFETIME);
        self.storage_access.retain(|grant| {
            !grant.has_expired() &&
                !(grant.top_level_site == top_level_site_name &&
                    grant.embedded_site == embedded_site)
        });
        self.storage_access.push(StorageAccessGrant {
            top_level_site: top_level_site_name,
            embedded_site,
            expires,
        });
    }

    pub fn delete_cookie_with_name(
        &mut self,
        url: &ServoUrl,
        name: String,
        partition: &CookiePartition,
    ) {
        let domain = reg_host(url.host_str().unwrap_or(""));
        if let Some(cookies) = self.cookies_map.get_mut(&domain) {
            for cookie in cookies.iter_mut() {
                if cookie.cookie.name() == name && is_in_partition(cookie, partition) {
                    cookie.set_expiry_time_in_past();
                }
            }
        }
    }

    // http://tools.ietf.org/html/rfc6265#section-5.3
    pub fn push(&mut self, mut cookie: ServoCookie, url: &ServoUrl, source: CookieSource) {
        if self.rejects_all {
            return;
        }

        // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt Step 1
        if cookie.cookie.secure().unwrap_or(false) && !url.is_secure_scheme() {
            return;
        }

        let old_cookie = self.remove(&cookie, url, source);
        if old_cookie.is_err() {
            // This new cookie is not allowed to overwrite an existing one.
            return;
        }

        // Step 11
        if let Some(old_cookie) = old_cookie.unwrap() {
            // Step 11.3
            cookie.creation_time = old_cookie.creation_time;
        }

        // Step 12
        let domain = reg_host(cookie.cookie.domain().as_ref().unwrap_or(&""));
        let cookies = self.cookies_map.entry(domain).or_default();

        // The limit is per partition, so that a site embedded by many others cannot have its
        // own cookies evicted by those it has elsewhere.
        let top_level_site = cookie.top_level_site.as_deref();
        let in_partition = |cookies: &Vec<ServoCookie>| {
            cookies
                .iter()
                .filter(|c| c.top_level_site.as_deref() == top_level_site)
                .count()
        };
        if in_partition(cookies) >= self.max_per_host {
            let old_len = in_partition(cookies);
            cookies.retain(|c| !is_cookie_expired(c));
            let new_len = in_partition(cookies);

            // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt
            if new_len == old_len &&
                !evict_one_cookie(
                    cookie.cookie.secure().unwrap_or(false),
                    top_level_site,
                    cookies,
                )
            {
                return;
            }
        }
        cookies.push(cookie);
    }

    pub fn cookie_comparator(a: &ServoCookie, b: &ServoCookie) -> Ordering {
        let a_path_len = a.cookie.path().as_ref().map_or(0, |p| p.len());
        let b_path_len = b.cookie.path().as_ref().map_or(0, |p| p.len());
        match a_path_len.cmp(&b_path_len) {
            Ordering::Equal => a.creation_time.cmp(&b.creation_time),
            // Ensure that longer paths are sorted earlier than shorter paths
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
        }
    }

    pub fn remove_expired_cookies_for_url(&mut self, url: &ServoUrl) {
        let domain = reg_host(url.host_str().unwrap_or(""));
        if let Entry::Occupied(mut entry) = self.cookies_map.entry(domain) {
            let cookies = entry.get_mut();
            cookies.retain(|c| !is_cookie_expired(c));
            if cookies.is_empty() {
                entry.remove_entry();
            }
        }
    }

    pub fn remove_all_expired_cookies(&mut self) {
        self.cookies_map.retain(|_, cookies| {
            cookies.retain(|c| !is_cookie_expired(c));
            !cookies.is_empty()
        });
    }

    // http://tools.ietf.org/html/rfc6265#section-5.4
    pub fn cookies_for_url(
        &mut self,
        url: &ServoUrl,
        source: CookieSource,
        partition: &CookiePartition,
    ) -> Option<String> {
        // Let cookie-list be the set of cookies from the cookie store
        let cookie_list = self.cookies_data_for_url(url, source, partition);

        let reducer = |acc: String, cookie: Cookie<'static>| -> String {
            // Serialize the cookie-list into a cookie-string by processing each cookie in the cookie-list in order:
            // If the cookies' name is not empty, output the cookie's name followed by the %x3D ("=") character.
            // If the cookies' value is not empty, output the cookie's value.
            // If there is an unprocessed cookie in the cookie-list, output the characters %x3B and %x20 ("; ").
            // Security: the above steps allow for "nameless" cookies which have proved to be a security footgun
            // especially with the new cookie name prefix proposals
            (match acc.len() {
                0 => acc,
                _ => acc + "; ",
            }) + cookie.name() +
                "=" +
                cookie.value()
        };

        // Serialize the cookie-list into a cookie-string by processing each cookie in the cookie-list in order
        let result = cookie_list.fold(String::new(), reducer);

        info!(" === COOKIES SENT: {}", result);
        match result.len() {
            0 => None,
            _ => Some(result),
        }
    }

    /// <https://cookiestore.spec.whatwg.org/#query-cookies>
    pub fn query_cookies(
        &mut self,
        url: &ServoUrl,
        name: Option<String>,
        partition: &CookiePartition,
    ) -> Vec<Cookie<'static>> {
        // 1. Retrieve cookie-list given request-uri and "non-HTTP" source
        let cookie_list = self.cookies_data_for_url(url, CookieSource::NonHTTP, partition);

        // 3. For each cookie in cookie-list, run these steps:
        // 3.2. If name is given, then run these steps:
        if let Some(name) = name {
            // Let cookieName be the result of running UTF-8 decode without BOM on cookie’s name.
            // If cookieName does not equal name, then continue.
            cookie_list.filter(|cookie| cookie.name() == name).collect()
        } else {
            cookie_list.collect()
        }

        // Note: we do not convert the list into CookieListItem's here, we do that in script to not not have to define
        // the binding types in net.

        // Return list
    }

    pub fn cookies_data_for_url<'a>(
        &'a mut self,
        url: &'a ServoUrl,
        source: CookieSource,
        partition: &'a CookiePartition,
    ) -> impl Iterator<Item = cookie::Cookie<'static>> + 'a {
        let domain = reg_host(url.host_str().unwrap_or(""));
        let cookies = self.cookies_map.entry(domain).or_default();

        cookies
            .iter_mut()
            .filter(move |c| is_in_partition(c, partition) && c.appropriate_for_url(url, source))
            .sorted_by(|a: &&mut ServoCookie, b: &&mut ServoCookie| {
                // The user agent SHOULD sort the cookie-list
                CookieStorage::cookie_comparator(a, b)
            })
            .map(|c| {
                // Update the last-access-time of each cookie in the cookie-list to the current date and time
                c.touch();
                c.cookie.clone()
            })
    }

    pub fn cookie_site_descriptors(&self) -> Vec<SiteDescriptor> {
        // A lookup leaves an empty entry behind for every host it asks about,
        // and removing the last cookie of a host does the same. Neither is a
        // site with data.
        self.cookies_map
            .iter()
            .filter(|(_, cookies)| !cookies.is_empty())
            .map(|(site, _)| SiteDescriptor::new(site.clone()))
            .collect()
    }
}

fn reg_host(url: &str) -> String {
    let host_for_ip_parse = url
        .strip_prefix('[')
        .and_then(|url| url.strip_suffix(']'))
        .unwrap_or(url);
    if let Ok(address) = host_for_ip_parse.parse::<IpAddr>() {
        debug_assert!(address.to_string() == address.to_string().to_lowercase());
        return address.to_string();
    }

    reg_suffix(url).to_lowercase()
}

/// Whether `cookie` is one of those `partition` uses. A [`CookiePartition::Refused`] has none.
fn is_in_partition(cookie: &ServoCookie, partition: &CookiePartition) -> bool {
    *partition != CookiePartition::Refused &&
        cookie.top_level_site.as_deref() == partition.top_level_site()
}

fn is_cookie_expired(cookie: &ServoCookie) -> bool {
    matches!(cookie.expiry_time, Some(date_time) if date_time <= SystemTime::now())
}

fn evict_one_cookie(
    is_secure_cookie: bool,
    top_level_site: Option<&str>,
    cookies: &mut Vec<ServoCookie>,
) -> bool {
    // Remove non-secure cookie with oldest access time
    let oldest_accessed = get_oldest_accessed(false, top_level_site, cookies);

    if let Some((index, _)) = oldest_accessed {
        cookies.remove(index);
    } else {
        // All secure cookies were found
        if !is_secure_cookie {
            return false;
        }
        let oldest_accessed = get_oldest_accessed(true, top_level_site, cookies);
        if let Some((index, _)) = oldest_accessed {
            cookies.remove(index);
        }
    }
    true
}

/// The oldest accessed of `cookies` that is secure or not as asked, and in the partition of
/// `top_level_site`.
fn get_oldest_accessed(
    is_secure_cookie: bool,
    top_level_site: Option<&str>,
    cookies: &mut [ServoCookie],
) -> Option<(usize, SystemTime)> {
    let mut oldest_accessed = None;
    for (i, c) in cookies.iter().enumerate() {
        if (c.cookie.secure().unwrap_or(false) == is_secure_cookie) &&
            c.top_level_site.as_deref() == top_level_site &&
            oldest_accessed
                .as_ref()
                .is_none_or(|(_, current_oldest_time)| c.last_access < *current_oldest_time)
        {
            oldest_accessed = Some((i, c.last_access));
        }
    }
    oldest_accessed
}
