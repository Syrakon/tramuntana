/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Which cookies a site has where it is embedded.
//!
//! A site keeps one set of cookies for when it is the top-level site, and it shares them with
//! its own frames. Embedded by another site, as an `<iframe>` or a subresource, it gets
//! instead a set of cookies kept for that top-level site alone, apart from its own: a site
//! embedded by many cannot recognise the user from one to the next. This is what Firefox does
//! as Total Cookie Protection, and it covers every cookie, not only those set as `Partitioned`
//! (<https://github.com/privacycg/CHIPS>).

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_url::{Host, ImmutableOrigin, ServoUrl};

use crate::pub_domains::{is_same_site, reg_suffix};

/// Where a request or a script asking for cookies comes from: the top-level origin of its
/// environment, and whether a frame between it and the top-level document is of another site.
#[derive(Clone, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct CookieContext {
    /// <https://html.spec.whatwg.org/multipage/#concept-environment-top-level-origin>
    pub top_level_origin: ImmutableOrigin,
    /// Whether the document, or one of the frames that embed it, is of a site other than
    /// that of `top_level_origin`. A site embedded in another site that embeds the first one
    /// again is then not first-party there.
    pub has_cross_site_ancestor: bool,
}

impl CookieContext {
    /// The context of a document of `origin`, embedded by a document whose context is
    /// `parent`, or top-level if there is none.
    pub fn of_document(origin: &ImmutableOrigin, parent: Option<&CookieContext>) -> Self {
        match parent {
            None => CookieContext {
                top_level_origin: origin.clone(),
                has_cross_site_ancestor: false,
            },
            Some(parent) => CookieContext {
                top_level_origin: parent.top_level_origin.clone(),
                has_cross_site_ancestor: parent.has_cross_site_ancestor ||
                    !is_same_site(origin, &parent.top_level_origin),
            },
        }
    }
}

/// The set of cookies a request or a script uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookiePartition {
    /// The cookies the site has as a top-level site.
    FirstParty,
    /// The cookies the site has when embedded by the top-level site named here, as
    /// [`top_level_site`] gives it.
    Embedded(String),
    /// No cookies at all: the top-level document has an opaque origin, so there is no site
    /// to keep them for.
    Refused,
}

impl CookiePartition {
    /// The cookies for `url` when used from `context`. Without a context, the request is a
    /// top-level navigation or one of the browser itself, whose cookies are first-party.
    pub fn for_url(url: &ServoUrl, context: Option<&CookieContext>) -> CookiePartition {
        let Some(context) = context else {
            return CookiePartition::FirstParty;
        };
        if !servo_config::pref!(network_cookie_partitioning_enabled) {
            return CookiePartition::FirstParty;
        }
        if !context.has_cross_site_ancestor &&
            is_same_site(&url.origin(), &context.top_level_origin)
        {
            return CookiePartition::FirstParty;
        }
        match top_level_site(&context.top_level_origin) {
            Some(site) => CookiePartition::Embedded(site),
            None => CookiePartition::Refused,
        }
    }

    /// The top-level site a cookie in this partition is stored under, `None` for a
    /// first-party one.
    pub fn top_level_site(&self) -> Option<&str> {
        match self {
            CookiePartition::Embedded(site) => Some(site),
            CookiePartition::FirstParty | CookiePartition::Refused => None,
        }
    }
}

/// The site of `origin`, its scheme and registered domain, as cookies are partitioned by it:
/// `https://example.com` for `https://www.example.com:8443`. `None` for an opaque origin.
pub fn top_level_site(origin: &ImmutableOrigin) -> Option<String> {
    let ImmutableOrigin::Tuple(scheme, host, _) = origin else {
        return None;
    };
    Some(match host {
        Host::Domain(domain) => format!("{scheme}://{}", reg_suffix(domain).to_lowercase()),
        address => format!("{scheme}://{address}"),
    })
}
