/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A file a page hands to the user agent to save, instead of a document to show.

use std::fmt::{Debug, Formatter};

use crossbeam_channel::Receiver;
use url::Url;

/// What arrives for a [`Download`], in order: any number of [`DownloadEvent::Data`],
/// then exactly one [`DownloadEvent::Finished`] or [`DownloadEvent::Failed`].
#[derive(Debug)]
pub enum DownloadEvent {
    /// The next bytes of the file.
    Data(Vec<u8>),
    /// The whole file has arrived.
    Finished,
    /// The transfer stopped before the end, or was cancelled; what arrived is incomplete.
    Failed(String),
}

/// A navigation whose response is a file to save rather than a document to render.
/// The navigation itself ends and the page stays as it was; the body of the
/// response arrives on [`Download::events`] as the network delivers it.
pub struct Download {
    /// The URL the file came from, after redirects.
    pub url: Url,
    /// The file name the server suggested in `Content-Disposition`, if any. It comes
    /// from the site: it may hold path separators or name a dangerous type.
    pub suggested_file_name: Option<String>,
    /// The MIME type the server declared, if any.
    pub mime_type: Option<String>,
    /// The length the server announced, if any. The body may still differ from it.
    pub expected_length: Option<u64>,
    /// The body of the file and how the transfer ended.
    pub events: Receiver<DownloadEvent>,
    cancel: Box<dyn Fn() + Send>,
}

impl Download {
    pub fn new(
        url: Url,
        suggested_file_name: Option<String>,
        mime_type: Option<String>,
        expected_length: Option<u64>,
        events: Receiver<DownloadEvent>,
        cancel: Box<dyn Fn() + Send>,
    ) -> Self {
        Self {
            url,
            suggested_file_name,
            mime_type,
            expected_length,
            events,
            cancel,
        }
    }

    /// Stop the transfer. [`Download::events`] then ends with [`DownloadEvent::Failed`].
    pub fn cancel(&self) {
        (self.cancel)()
    }
}

impl Debug for Download {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("url", &self.url)
            .field("suggested_file_name", &self.suggested_file_name)
            .field("mime_type", &self.mime_type)
            .field("expected_length", &self.expected_length)
            .finish_non_exhaustive()
    }
}
