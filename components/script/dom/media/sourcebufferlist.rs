/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::window::Window;

/// <https://w3c.github.io/media-source/#sourcebufferlist>
#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    source_buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    fn new_inherited() -> SourceBufferList {
        SourceBufferList {
            eventtarget: EventTarget::new_inherited(),
            source_buffers: DomRefCell::new(Vec::new()),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<SourceBufferList> {
        reflect_dom_object(cx, Box::new(SourceBufferList::new_inherited()), window)
    }

    pub(crate) fn item(&self, index: usize) -> Option<DomRoot<SourceBuffer>> {
        self.source_buffers
            .borrow()
            .get(index)
            .map(|source_buffer| DomRoot::from_ref(&**source_buffer))
    }

    pub(crate) fn contains(&self, source_buffer: &SourceBuffer) -> bool {
        self.source_buffers
            .borrow()
            .iter()
            .any(|other| &**other == source_buffer)
    }

    /// The source buffers of the list. The list can change while they are used.
    pub(crate) fn to_vec(&self) -> Vec<DomRoot<SourceBuffer>> {
        self.source_buffers
            .borrow()
            .iter()
            .map(|source_buffer| DomRoot::from_ref(&**source_buffer))
            .collect()
    }

    /// Append the source buffer and queue a task to fire `addsourcebuffer`.
    pub(crate) fn add(&self, source_buffer: &SourceBuffer) {
        if self.contains(source_buffer) {
            return;
        }
        self.source_buffers
            .borrow_mut()
            .push(Dom::from_ref(source_buffer));
        self.queue_event("addsourcebuffer");
    }

    /// Remove the source buffer, if it is in the list, and queue a task to fire
    /// `removesourcebuffer`.
    pub(crate) fn remove(&self, source_buffer: &SourceBuffer) {
        if !self.contains(source_buffer) {
            return;
        }
        self.source_buffers
            .borrow_mut()
            .retain(|other| &**other != source_buffer);
        self.queue_event("removesourcebuffer");
    }

    /// Remove every source buffer and queue a single task to fire
    /// `removesourcebuffer`, if there was any.
    pub(crate) fn clear(&self) {
        if self.source_buffers.borrow().is_empty() {
            return;
        }
        self.source_buffers.borrow_mut().clear();
        self.queue_event("removesourcebuffer");
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_list_event: move |cx| {
                let this = this.root();
                this.upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.source_buffers.borrow().len() as u32
    }

    /// <https://w3c.github.io/media-source/#dfn-sourcebufferlist-getter>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.item(index as usize)
    }

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onaddsourcebuffer
    event_handler!(
        addsourcebuffer,
        GetOnaddsourcebuffer,
        SetOnaddsourcebuffer
    );

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onremovesourcebuffer
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );
}
