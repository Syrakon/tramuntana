/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt::Debug;
use std::hash::Hash;
use std::path::PathBuf;
use std::time::SystemTime;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use servo_base::Epoch;
use servo_base::generic_channel::GenericSender;
use servo_base::id::{PipelineId, WebViewId};
use servo_url::ImmutableOrigin;
use url::Url;
use uuid::Uuid;

use crate::{InputMethodType, RgbColor};

/// The id of a user interface control that the engine requests that the
/// embedder show.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct EmbedderControlId {
    #[doc(hidden)]
    pub webview_id: WebViewId,
    #[doc(hidden)]
    pub pipeline_id: PipelineId,
    #[doc(hidden)]
    pub index: Epoch,
}

/// A request from the engine to the embedder to display a user interface control.
#[derive(Debug, Deserialize, Serialize)]
pub enum EmbedderControlRequest {
    /// Indicates that the user has activated a `<select>` element.
    SelectElement(SelectElementRequest),
    /// Indicates that the user has activated a `<input type=color>` element. It carries the
    /// element's color and, when the element has an `alpha` attribute, its opacity from 0 to 1.
    ColorPicker(RgbColor, Option<f32>),
    /// Indicates that the user has activated a `<input type=file>` element.
    FilePicker(FilePickerRequest),
    /// Indicates that the user has activated a `<input type=date>`, `<input type=month>`,
    /// `<input type=datetime-local>` or `<input type=time>` element.
    DatePicker(DatePickerRequest),
    /// Indicates that the the user has activated a text or input control that should show
    /// an IME.
    InputMethod(InputMethodRequest),
    /// Indicates that the the user has triggered the display of a context menu.
    ContextMenu(ContextMenuRequest),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SelectElementOption {
    /// A unique identifier for the option that can be used to select it.
    pub id: usize,
    /// The label that should be used to display the option to the user.
    pub label: String,
    /// Whether or not the option is selectable
    pub is_disabled: bool,
}

/// Represents the contents of either an `<option>` or an `<optgroup>` element
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SelectElementOptionOrOptgroup {
    Option(SelectElementOption),
    Optgroup {
        label: String,
        options: Vec<SelectElementOption>,
    },
}

/// Request to present a context menu to the user. This is triggered by things like
/// right-clicking on web content.
#[derive(Debug, Deserialize, Serialize)]
pub struct ContextMenuRequest {
    pub element_info: ContextMenuElementInformation,
    pub items: Vec<ContextMenuItem>,
}

/// An item in a context menu.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ContextMenuItem {
    Item {
        label: String,
        action: ContextMenuAction,
        enabled: bool,
    },
    Separator,
}

/// A particular action associated with a [`ContextMenuItem`]. These actions are
/// context-sensitive, which means that some of them are available only for some
/// page elements.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ContextMenuAction {
    GoBack,
    GoForward,
    Reload,

    CopyLink,
    OpenLinkInNewWebView,

    CopyImageLink,
    OpenImageInNewView,

    Cut,
    Copy,
    Paste,
    SelectAll,
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
    pub struct ContextMenuElementInformationFlags: u8 {
        /// Whether or not the element this context menu was activated for was a link.
        const Link = 1 << 1;
        /// Whether or not the element this context menu was activated for was an image.
        const Image = 1 << 2;
        /// Whether or not the element this context menu was activated for was editable
        /// text.
        const EditableText = 1 << 3;
        /// Whether or not the element this context menu was activated for was covered by
        /// a selection.
        const Selection = 1 << 4;
    }
}

/// Information about the element that a context menu was activated for. values which
/// do not apply to this element will be `None`.
///
/// Note that an element might be both an image and a link, if the element is an `<img>`
/// tag nested inside of a `<a>` tag.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ContextMenuElementInformation {
    pub flags: ContextMenuElementInformationFlags,
    pub link_url: Option<Url>,
    pub image_url: Option<Url>,
}

/// Request to present an IME to the user when an editable element is focused. If `type` is
/// [`InputMethodType::Text`], then the `text` parameter specifies the pre-existing text content and
/// `insertion_point` the zero-based index into the string of the insertion point.
#[derive(Debug, Deserialize, Serialize)]
pub struct InputMethodRequest {
    pub input_method_type: InputMethodType,
    pub text: String,
    pub insertion_point: Option<u32>,
    pub multiline: bool,
    pub allow_virtual_keyboard: bool,
}

/// Filter for file selection;
/// the `String` content is expected to be extension (e.g, "doc", without the prefixing ".")
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FilterPattern(pub String);

#[derive(Debug, Deserialize, Serialize)]
pub struct FilePickerRequest {
    pub origin: ImmutableOrigin,
    pub current_paths: Vec<PathBuf>,
    pub filter_patterns: Vec<FilterPattern>,
    pub allow_select_multiple: bool,
    pub accept_current_paths_for_testing: bool,
}

/// The kind of value a [`DatePickerRequest`] asks the user to pick. Each kind uses the
/// value format of its `<input>` type.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum DatePickerKind {
    /// `<input type=date>`: a [date string](https://html.spec.whatwg.org/multipage/#valid-date-string),
    /// such as `2026-11-02`.
    Date,
    /// `<input type=month>`: a [month string](https://html.spec.whatwg.org/multipage/#valid-month-string),
    /// such as `2026-11`.
    Month,
    /// `<input type=datetime-local>`: a
    /// [normalized local date and time string](https://html.spec.whatwg.org/multipage/#valid-normalised-local-date-and-time-string),
    /// such as `2026-11-02T14:30`.
    DatetimeLocal,
    /// `<input type=time>`: a [time string](https://html.spec.whatwg.org/multipage/#valid-time-string),
    /// such as `14:30` or `14:30:15`. The picker shows no calendar, only a clock.
    Time,
}

/// Request to the embedder to display a calendar for a date-like `<input>` element.
/// All the strings use the value format of [`DatePickerRequest::kind`]; any of them
/// may be empty when the element has no valid value for it.
#[derive(Debug, Deserialize, Serialize)]
pub struct DatePickerRequest {
    pub kind: DatePickerKind,
    /// The current value of the element.
    pub value: String,
    /// The value of the `min` attribute, if it is valid.
    pub min: Option<String>,
    /// The value of the `max` attribute, if it is valid.
    pub max: Option<String>,
    /// For the kinds that hold a time, the
    /// [allowed value step](https://html.spec.whatwg.org/multipage/#concept-input-step) in
    /// seconds, or `None` when the element allows any step (`step=any`). A step that is not
    /// a whole number of minutes means the picker should offer seconds.
    pub step: Option<f64>,
}

/// Request to the embedder to display a control for a `<select>` element.
#[derive(Debug, Deserialize, Serialize)]
pub struct SelectElementRequest {
    pub options: Vec<SelectElementOptionOrOptgroup>,
    pub selected_options: Vec<usize>,
    pub allow_select_multiple: bool,
}

/// Response from the embedder to an [`EmbedderControlRequest`].
#[derive(Debug, Deserialize, Serialize)]
pub enum EmbedderControlResponse {
    SelectElement(Vec<usize>),
    /// The color the user picked, or `None` to leave the element as it is, and the opacity
    /// picked, from 0 to 1, when the request carried one.
    ColorPicker(Option<RgbColor>, Option<f32>),
    FilePicker(Option<Vec<SelectedFile>>),
    /// The value the user picked, in the format of the request's [`DatePickerKind`], or
    /// `None` if the picker was dismissed.
    DatePicker(Option<String>),
    ContextMenu(Option<ContextMenuAction>),
}

/// Response to file selection request
#[derive(Debug, Deserialize, Serialize)]
pub struct SelectedFile {
    pub id: Uuid,
    pub filename: PathBuf,
    pub modified: SystemTime,
    pub size: u64,
    // https://w3c.github.io/FileAPI/#dfn-type
    pub type_string: String,
}

/// Request from Servo to the embedder with the details of the simple dialog to be displayed.
#[derive(Deserialize, Serialize)]
pub enum SimpleDialogRequest {
    Alert {
        id: EmbedderControlId,
        message: String,
        response_sender: GenericSender<AlertResponse>,
    },
    Confirm {
        id: EmbedderControlId,
        message: String,
        response_sender: GenericSender<ConfirmResponse>,
    },
    Prompt {
        id: EmbedderControlId,
        message: String,
        default: String,
        response_sender: GenericSender<PromptResponse>,
    },
}

/// The action selected by the user in the alert dialog.
#[derive(Deserialize, PartialEq, Serialize)]
pub enum AlertResponse {
    Ok,
}

/// The action selected by the user in the confirm dialog.
#[derive(Deserialize, PartialEq, Serialize)]
pub enum ConfirmResponse {
    Ok,
    Cancel,
}

/// The action selected by the user in the prompt dialog.
#[derive(Deserialize, PartialEq, Serialize)]
pub enum PromptResponse {
    Ok(String),
    Cancel,
}
