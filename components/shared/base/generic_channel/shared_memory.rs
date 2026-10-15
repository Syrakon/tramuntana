/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use ipc_channel::ipc::IpcSharedMemory;
use malloc_size_of::MallocSizeOf;
use serde::de::VariantAccess;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::generic_channel::use_ipc;

#[derive(Clone)]
pub struct GenericSharedMemory(GenericSharedMemoryVariant);

#[derive(Clone)]
enum GenericSharedMemoryVariant {
    Ipc(IpcSharedMemory),
    InProcess(Arc<Vec<u8>>),
}

impl AsRef<[u8]> for GenericSharedMemory {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Deref for GenericSharedMemory {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match &self.0 {
            GenericSharedMemoryVariant::Ipc(ipc_shared_memory) => ipc_shared_memory,
            GenericSharedMemoryVariant::InProcess(items) => items.as_slice(),
        }
    }
}

impl MallocSizeOf for GenericSharedMemory {
    fn size_of(&self, ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        match &self.0 {
            GenericSharedMemoryVariant::Ipc(_) => 0,
            GenericSharedMemoryVariant::InProcess(items) => items.size_of(ops),
        }
    }
}

impl GenericSharedMemory {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if use_ipc() {
            GenericSharedMemory(GenericSharedMemoryVariant::Ipc(
                IpcSharedMemory::from_bytes(bytes),
            ))
        } else {
            GenericSharedMemory(GenericSharedMemoryVariant::InProcess(Arc::new(
                bytes.to_owned(),
            )))
        }
    }

    pub fn from_byte(data: u8, length: usize) -> Self {
        if use_ipc() {
            GenericSharedMemory(GenericSharedMemoryVariant::Ipc(IpcSharedMemory::from_byte(
                data, length,
            )))
        } else {
            GenericSharedMemory(GenericSharedMemoryVariant::InProcess(Arc::new(vec![
                data;
                length
            ])))
        }
    }

    /// Build a `GenericSharedMemory` from a `Vec<u8>`.
    ///
    /// In single-process mode this allows reusing the Vec and the only cost is
    /// allocating a new Arc. Prefer over `Self::from_bytes` if ownership is
    /// transferred.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        if use_ipc() {
            GenericSharedMemory(GenericSharedMemoryVariant::Ipc(
                IpcSharedMemory::from_bytes(&bytes),
            ))
        } else {
            GenericSharedMemory(GenericSharedMemoryVariant::InProcess(Arc::new(bytes)))
        }
    }

    /// Build a `GenericSharedMemory` that stays in this process's heap, even in multiprocess
    /// mode. It cannot be sent to another process as is: send [`Self::sendable`] instead.
    ///
    /// For data that is kept around and only sometimes sent, where shared memory would cost
    /// more than it saves: a file descriptor and a page-aligned mapping for every value.
    pub fn from_vec_in_process(bytes: Vec<u8>) -> Self {
        GenericSharedMemory(GenericSharedMemoryVariant::InProcess(Arc::new(bytes)))
    }

    /// Whether this value lives in memory that another process can map, so that sending it
    /// shares the memory instead of copying it.
    pub fn is_cross_process(&self) -> bool {
        matches!(self.0, GenericSharedMemoryVariant::Ipc(_))
    }

    /// A value that can be sent to another process: a clone of this one when it can already be
    /// sent, which shares the memory, otherwise a copy in shared memory.
    pub fn to_sendable(&self) -> Self {
        match &self.0 {
            GenericSharedMemoryVariant::InProcess(bytes) if use_ipc() => Self::from_bytes(bytes),
            _ => self.clone(),
        }
    }

    /// A value that can be sent to another process: `this` itself when it can already be sent,
    /// otherwise a copy in shared memory.
    pub fn sendable(this: &Arc<Self>) -> Arc<Self> {
        match &this.0 {
            GenericSharedMemoryVariant::InProcess(bytes) if use_ipc() => {
                Arc::new(Self::from_bytes(bytes))
            },
            _ => Arc::clone(this),
        }
    }

    /// Build a `GenericSharedMemory` from an `Arc<Vec<u8>>`.
    ///
    /// In single-process mode this allows creating shared memory without copying.
    pub fn from_arc_vec(arc: Arc<Vec<u8>>) -> Self {
        if use_ipc() {
            GenericSharedMemory(GenericSharedMemoryVariant::Ipc(
                IpcSharedMemory::from_bytes(&arc),
            ))
        } else {
            GenericSharedMemory(GenericSharedMemoryVariant::InProcess(arc))
        }
    }

    /// Free operation in single process mode.
    /// If multiple `GenericSharedmemory` point to the same value this is safe to use and only effects the value currently hold.
    pub fn into_arc_vec(self) -> Arc<Vec<u8>> {
        match self.0 {
            GenericSharedMemoryVariant::Ipc(ipc_shared_memory) => {
                Arc::new(ipc_shared_memory.to_vec())
            },
            GenericSharedMemoryVariant::InProcess(arc) => arc,
        }
    }

    pub fn from_bytes_with_mutator(bytes: &[u8], mutator: impl FnOnce(&mut [u8])) -> Self {
        let mut shared_memory = Self::from_bytes(bytes);
        match &mut shared_memory.0 {
            GenericSharedMemoryVariant::Ipc(ipc_shared_memory) => {
                #[expect(unsafe_code)]
                unsafe {
                    mutator(ipc_shared_memory.deref_mut())
                }
            },
            GenericSharedMemoryVariant::InProcess(arc) => mutator(
                Arc::get_mut(arc)
                    .expect("Arc just created from bytes")
                    .as_mut_slice(),
            ),
        }
        shared_memory
    }

    #[expect(unsafe_code)]
    /// Returns a mutable reference to the deref of this IpcSharedMemory.
    ///
    /// # Safety
    ///
    /// This is safe if there is only one reader/writer on the data. In single process mode breaking invariant causes panics.
    pub unsafe fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.0 {
            GenericSharedMemoryVariant::Ipc(ipc_shared_memory) => {
                #[expect(unsafe_code)]
                unsafe {
                    ipc_shared_memory.deref_mut()
                }
            },
            GenericSharedMemoryVariant::InProcess(arc) => Arc::get_mut(arc)
                .expect("Arc just created from bytes")
                .as_mut_slice(),
        }
    }
}

impl fmt::Debug for GenericSharedMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GenericSharedMemory").finish()
    }
}

impl Serialize for GenericSharedMemory {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            GenericSharedMemoryVariant::Ipc(memory) => {
                s.serialize_newtype_variant("GenericSharedMemory", 0, "Ipc", memory)
            },
            GenericSharedMemoryVariant::InProcess(arc) => {
                if use_ipc() {
                    return Err(serde::ser::Error::custom(
                        "Arc<Vec<u8>> found in multiprocess mode!",
                    ));
                } // We know everything is in one address-space, so we can "serialize" the receiver by
                // sending a leaked Arc pointer.
                let address = Arc::into_raw(arc.clone()) as *mut Vec<u8> as usize;
                s.serialize_newtype_variant("GenericSharedMemory", 1, "InProcess", &address)
            },
        }
    }
}

struct GenericSharedMemoryVisitor {}

impl<'de> serde::de::Visitor<'de> for GenericSharedMemoryVisitor {
    type Value = GenericSharedMemory;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a GenericReceiver variant")
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::EnumAccess<'de>,
    {
        #[derive(Deserialize)]
        enum GenericSharedMemoryVariantNames {
            Ipc,
            InProcess,
        }

        let (variant_name, variant_data): (GenericSharedMemoryVariantNames, _) = data.variant()?;

        match variant_name {
            GenericSharedMemoryVariantNames::Ipc => variant_data
                .newtype_variant::<IpcSharedMemory>()
                .map(|receiver| GenericSharedMemory(GenericSharedMemoryVariant::Ipc(receiver))),
            GenericSharedMemoryVariantNames::InProcess => {
                if use_ipc() {
                    return Err(serde::de::Error::custom(
                        "Arc data found in multiprocess mode!",
                    ));
                }
                let addr = variant_data.newtype_variant::<usize>()?;
                let ptr = addr as *mut Vec<u8>;
                // SAFETY: We know we are in the same address space as the sender, so we can safely
                // reconstruct the Arc.
                #[expect(unsafe_code)]
                let arc = unsafe { Arc::from_raw(ptr) };
                Ok(GenericSharedMemory(GenericSharedMemoryVariant::InProcess(
                    arc,
                )))
            },
        }
    }
}

impl<'a> Deserialize<'a> for GenericSharedMemory {
    fn deserialize<D>(d: D) -> Result<GenericSharedMemory, D::Error>
    where
        D: Deserializer<'a>,
    {
        d.deserialize_enum(
            "GenericSharedMemory",
            &["Ipc", "InProcess"],
            GenericSharedMemoryVisitor {},
        )
    }
}

#[cfg(test)]
mod single_process_shared_memory_test {
    use std::sync::Arc;

    use ipc_channel::ipc::IpcSharedMemory;

    use super::GenericSharedMemory;
    use crate::generic_channel::{self};

    #[test]
    fn test_ipc() {
        let bytes = vec![0xba; 10];
        let bytes_copy = bytes.clone();
        let shared_memory = GenericSharedMemory(super::GenericSharedMemoryVariant::Ipc(
            IpcSharedMemory::from_bytes(&bytes),
        ));

        let (send, recv) = generic_channel::channel().unwrap();
        send.send(shared_memory).expect("Could not send");
        assert_eq!(recv.recv().unwrap().to_vec(), bytes_copy);
    }

    #[test]
    fn test_inprocess() {
        let bytes = vec![0xba; 10];
        let bytes_copy = bytes.clone();
        let shared_memory = GenericSharedMemory(super::GenericSharedMemoryVariant::InProcess(
            Arc::new(bytes.clone()),
        ));

        let (send, recv) = generic_channel::channel().unwrap();
        send.send(shared_memory).expect("Could not send");
        assert_eq!(recv.recv().unwrap().to_vec(), bytes_copy);
    }
}
