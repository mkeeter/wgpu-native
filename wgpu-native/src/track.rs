use crate::hub::{NewId, Id, Index, Epoch, Storage};
use crate::resource::{BufferUsageFlags, TextureUsageFlags};
use crate::{
    RefCount,
    BufferId, TextureId, TextureViewId,
};

use bitflags::bitflags;
use hal::backend::FastHashMap;

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::marker::PhantomData;
use std::mem;
use std::ops::{BitOr, Range};


#[derive(Clone, Debug, PartialEq)]
#[allow(unused)]
pub enum Tracktion<T> {
    Init,
    Keep,
    Extend { old: T },
    Replace { old: T },
}

impl<T> Tracktion<T> {
    pub fn into_source(self) -> Option<T> {
        match self {
            Tracktion::Init |
            Tracktion::Keep => None,
            Tracktion::Extend { old } |
            Tracktion::Replace { old } => Some(old),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query<T> {
    pub usage: T,
    pub initialized: bool,
}

bitflags! {
    pub struct TrackPermit: u32 {
        /// Allow extension of the current usage. This is useful during render pass
        /// recording, where the usage has to stay constant, but we can defer the
        /// decision on what it is until the end of the pass.
        const EXTEND = 1;
        /// Allow replacing the current usage with the new one. This is useful when
        /// recording a command buffer live, and the current usage is already been set.
        const REPLACE = 2;
    }
}

pub trait GenericUsage {
    fn is_exclusive(&self) -> bool;
}
impl GenericUsage for BufferUsageFlags {
    fn is_exclusive(&self) -> bool {
        BufferUsageFlags::WRITE_ALL.intersects(*self)
    }
}
impl GenericUsage for TextureUsageFlags {
    fn is_exclusive(&self) -> bool {
        TextureUsageFlags::WRITE_ALL.intersects(*self)
    }
}

#[derive(Clone)]
struct Track<U> {
    ref_count: RefCount,
    init: U,
    last: U,
    epoch: Epoch,
}

//TODO: consider having `I` as an associated type of `U`?
pub struct Tracker<I, U> {
    map: FastHashMap<Index, Track<U>>,
    _phantom: PhantomData<I>,
}
pub type BufferTracker = Tracker<BufferId, BufferUsageFlags>;
pub type TextureTracker = Tracker<TextureId, TextureUsageFlags>;
pub struct DummyTracker<I> {
    map: FastHashMap<Index, (RefCount, Epoch)>,
    _phantom: PhantomData<I>,
}
pub type TextureViewTracker = DummyTracker<TextureViewId>;

pub struct TrackerSet {
    pub buffers: BufferTracker,
    pub textures: TextureTracker,
    pub views: TextureViewTracker,
    //TODO: samplers
}

impl TrackerSet {
    pub fn new() -> Self {
        TrackerSet {
            buffers: BufferTracker::new(),
            textures: TextureTracker::new(),
            views: TextureViewTracker::new(),
        }
    }
}

impl<I: NewId> DummyTracker<I> {
    pub fn new() -> Self {
        DummyTracker {
            map: FastHashMap::default(),
            _phantom: PhantomData,
        }
    }

    /// Remove an id from the tracked map.
    pub(crate) fn remove(&mut self, id: I) -> bool {
        match self.map.remove(&id.index()) {
            Some((_, epoch)) => {
                assert_eq!(epoch, id.epoch());
                true
            }
            None => false,
        }
    }

    /// Get the last usage on a resource.
    pub(crate) fn query(&mut self, id: I, ref_count: &RefCount) -> bool {
        match self.map.entry(id.index()) {
            Entry::Vacant(e) => {
                e.insert((ref_count.clone(), id.epoch()));
                true
            }
            Entry::Occupied(e) => {
                assert_eq!(e.get().1, id.epoch());
                false
            }
        }
    }

    /// Consume another tacker.
    pub fn consume(&mut self, other: &Self) {
        for (&index, &(ref ref_count, epoch)) in &other.map {
            self.query(I::new(index, epoch), ref_count);
        }
    }
}

impl<I: NewId, U: Copy + GenericUsage + BitOr<Output = U> + PartialEq> Tracker<I, U> {
    pub fn new() -> Self {
        Tracker {
            map: FastHashMap::default(),
            _phantom: PhantomData,
        }
    }

    /// Remove an id from the tracked map.
    pub(crate) fn remove(&mut self, id: I) -> bool {
        match self.map.remove(&id.index()) {
            Some(track) => {
                assert_eq!(track.epoch, id.epoch());
                true
            }
            None => false,
        }
    }

    /// Get the last usage on a resource.
    pub(crate) fn query(&mut self, id: I, ref_count: &RefCount, default: U) -> Query<U> {
        match self.map.entry(id.index()) {
            Entry::Vacant(e) => {
                e.insert(Track {
                    ref_count: ref_count.clone(),
                    init: default,
                    last: default,
                    epoch: id.epoch(),
                });
                Query {
                    usage: default,
                    initialized: true,
                }
            }
            Entry::Occupied(e) => {
                assert_eq!(e.get().epoch, id.epoch());
                Query {
                    usage: e.get().last,
                    initialized: false,
                }
            }
        }
    }

    /// Transit a specified resource into a different usage.
    pub(crate) fn transit(
        &mut self,
        id: I,
        ref_count: &RefCount,
        usage: U,
        permit: TrackPermit,
    ) -> Result<Tracktion<U>, U> {
        match self.map.entry(id.index()) {
            Entry::Vacant(e) => {
                e.insert(Track {
                    ref_count: ref_count.clone(),
                    init: usage,
                    last: usage,
                    epoch: id.epoch(),
                });
                Ok(Tracktion::Init)
            }
            Entry::Occupied(mut e) => {
                assert_eq!(e.get().epoch, id.epoch());
                let old = e.get().last;
                if usage == old {
                    Ok(Tracktion::Keep)
                } else if permit.contains(TrackPermit::EXTEND) && !(old | usage).is_exclusive() {
                    e.get_mut().last = old | usage;
                    Ok(Tracktion::Extend { old })
                } else if permit.contains(TrackPermit::REPLACE) {
                    e.get_mut().last = usage;
                    Ok(Tracktion::Replace { old })
                } else {
                    Err(old)
                }
            }
        }
    }

    /// Consume another tacker, adding it's transitions to `self`.
    /// Transitions the current usage to the new one.
    pub fn consume_by_replace<'a>(&'a mut self, other: &'a Self) -> impl 'a + Iterator<Item = (I, Range<U>)> {
        other.map.iter().flat_map(move |(&index, new)| {
            match self.map.entry(index) {
                Entry::Vacant(e) => {
                    e.insert(new.clone());
                    None
                }
                Entry::Occupied(mut e) => {
                    assert_eq!(e.get().epoch, new.epoch);
                    let old = mem::replace(&mut e.get_mut().last, new.last);
                    if old == new.init {
                        None
                    } else {
                        Some((I::new(index, new.epoch), old .. new.last))
                    }
                }
            }
        })
    }

    /// Consume another tacker, adding it's transitions to `self`.
    /// Extends the current usage without doing any transitions.
    pub fn consume_by_extend<'a>(&'a mut self, other: &'a Self) -> Result<(), (I, Range<U>)> {
        for (&index, new) in other.map.iter() {
            match self.map.entry(index) {
                Entry::Vacant(e) => {
                    e.insert(new.clone());
                }
                Entry::Occupied(mut e) => {
                    assert_eq!(e.get().epoch, new.epoch);
                    let old = e.get().last;
                    if old != new.last {
                        let extended = old | new.last;
                        if extended.is_exclusive() {
                            let id = I::new(index, new.epoch);
                            return Err((id, old .. new.last));
                        }
                        e.get_mut().last = extended;
                    }
                }
            }
        }
        Ok(())
    }

    /// Return an iterator over used resources keys.
    pub fn used<'a>(&'a self) -> impl 'a + Iterator<Item = I> {
        self.map.iter().map(|(&index, track)| I::new(index, track.epoch))
    }
}

impl<U: Copy + GenericUsage + BitOr<Output = U> + PartialEq> Tracker<Id, U> {
    fn _get_with_usage<'a, T: 'a + Borrow<RefCount>>(
        &mut self,
        storage: &'a Storage<T>,
        id: Id,
        usage: U,
        permit: TrackPermit,
    ) -> Result<(&'a T, Tracktion<U>), U> {
        let item = storage.get(id);
        self.transit(id, item.borrow(), usage, permit)
            .map(|tracktion| (item, tracktion))
    }

    pub(crate) fn get_with_extended_usage<'a, T: 'a + Borrow<RefCount>>(
        &mut self,
        storage: &'a Storage<T>,
        id: Id,
        usage: U,
    ) -> Result<&'a T, U> {
        let item = storage.get(id);
        self.transit(id, item.borrow(), usage, TrackPermit::EXTEND)
            .map(|_tracktion| item)
    }

    pub(crate) fn get_with_replaced_usage<'a, T: 'a + Borrow<RefCount>>(
        &mut self,
        storage: &'a Storage<T>,
        id: Id,
        usage: U,
    ) -> Result<(&'a T, Option<U>), U> {
        let item = storage.get(id);
        self.transit(id, item.borrow(), usage, TrackPermit::REPLACE)
            .map(|tracktion| (item, match tracktion {
                Tracktion::Init |
                Tracktion::Keep => None,
                Tracktion::Extend { ..} => unreachable!(),
                Tracktion::Replace { old } => Some(old),
            }))
    }
}