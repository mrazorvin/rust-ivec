use core::panic;
use std::any::Any;
use std::cell::UnsafeCell;
use std::io::Empty;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::panic::RefUnwindSafe;
use std::ptr::addr_of;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, RwLockReadGuard};
use std::time::Duration;
use std::{hash::Hash, sync::RwLock};

use std::collections::HashMap;

use super::disposable::{DisposeItem, FrameDisposable};
use super::sync_vec::SyncVec;

enum SlotStateEnum {
    Deleted = 0,
    Stable = 1,
    Updated = 2,
}

pub struct SyncMap<Key: 'static, T: 'static> {
    dispose_target: *const SyncVec<DisposeItem>,
    need_disposing: AtomicBool,
    write: RwLock<HashMap<Key, Option<Arc<T>>>>,
    read: UnsafeCell<HashMap<Key, (T, AtomicU8)>>,
}

unsafe impl<Key, T> Send for SyncMap<Key, T> {}
unsafe impl<Key, T> Sync for SyncMap<Key, T> {}

#[derive(PartialEq, Debug)]
pub enum SyncMapSlot<'a, T> {
    Read(&'a T),
    Write(Arc<T>),
    Empty,
}

impl<Key, T> SyncMap<Key, T> {
    pub fn new(dispose_target: Option<*const SyncVec<DisposeItem>>) -> Self {
        SyncMap {
            dispose_target: dispose_target.unwrap_or(std::ptr::null()),
            need_disposing: AtomicBool::new(false),
            read: Default::default(),
            write: Default::default(),
        }
    }

    pub fn get<'a, 'b>(&'a self, k: &'b Key) -> SyncMapSlot<'a, T>
    where
        Key: Eq + Hash,
    {
        // SAFETY: Field {read} borrowed as mutable only inside `SyncMap.apply_updates()` fn
        //         This fn MUST be called only when not other thread can access SyncMap content
        //         This why immutable borrow for {read} field is always safe
        let value =
            unsafe { (*self.read.get()).get(k).map(|v| (&v.0, v.1.load(Ordering::Acquire))) };
        match value {
            Some((value, 1)) => SyncMapSlot::Read(value),
            _ => {
                let lock = self.write.read().expect("SyncMap.get# poison guards non supported");
                let result = match lock.get(k) {
                    Some(value) => match &value {
                        Some(ref arc) => SyncMapSlot::Write(Arc::clone(arc)),
                        None => SyncMapSlot::Empty,
                    },
                    _ => SyncMapSlot::Empty,
                };
                drop(lock);
                return result;
            }
        }
    }

    pub fn set(&self, k: Key, value: T)
    where
        Key: Eq + Hash,
    {
        self.request_disposing();

        // SAFETY: Field {read} borrowed as mutable only inside `SyncMap.apply_updates()` fn
        //         This fn MUST be called only when not other thread can access SyncMap content
        //         This why immutable borrow for {read} field is always safe
        let existed_value = unsafe { (*self.read.get()).get(&k) };
        let mut lock = self.write.write().expect("SyncMap.insert# poison guards non supported");
        lock.insert(k, Some(Arc::new(value)));
        drop(lock);
        match existed_value {
            Some((_, state)) if state.load(Ordering::Acquire) == (SlotStateEnum::Stable as u8) => {
                let _ = state.compare_exchange(
                    SlotStateEnum::Stable as u8,
                    SlotStateEnum::Updated as u8,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
            _ => {}
        };
    }

    fn delete(&self, k: Key)
    where
        Key: Eq + Hash,
    {
        self.request_disposing();

        // SAFETY: Field {read} borrowed as mutable only inside `SyncMap.apply_updates()` fn
        //         This fn can be used only when single thread can access map
        //         This why immutable borrow for {read} field is always safe
        let existed_value = unsafe { (*self.read.get()).get(&k) };
        let mut lock = self.write.write().expect("SyncMap.delete# poison guards non supported");
        match existed_value {
            Some((_, state)) => {
                if state.load(Ordering::Acquire) != (SlotStateEnum::Deleted as u8) {
                    lock.insert(k, None);
                    state.store(SlotStateEnum::Deleted as u8, Ordering::Release);
                }
            }
            _ => {
                lock.remove(&k);
            }
        };
        drop(lock);
    }

    fn request_disposing(&self)
    where
        Key: Eq + Hash,
    {
        if self.dispose_target.is_null() == false
            && self.need_disposing.load(Ordering::Acquire) == false
        {
            unsafe { (*self.dispose_target).push(DisposeItem { data: self }) };
            self.need_disposing.store(true, Ordering::Release);
        }
    }

    // This fn should be called ONLY when no other thread can access current `SyncMap`
    // and there no references and arc clones to `SyncMap` content
    // typicall use-case is functions scheduled by `World` inside `RenderLoop`
    unsafe fn apply_updates(&self)
    where
        Key: Eq + Hash,
    {
        let mut write_map =
            self.write.write().expect("SyncMap.appply_updates# poison guards non supported");
        let read_map = unsafe { &mut *self.read.get() };

        for (k, v) in write_map.drain() {
            match v {
                Some(value) => {
                    if Arc::strong_count(&value) > 1 {
                        unimplemented!(
                            "references to sync map slots outside of world cycle not supported"
                        );
                    }

                    let read_ptr = Arc::into_raw(value);
                    let value = unsafe { read_ptr.read() };
                    read_map.insert(k, (value, AtomicU8::new(SlotStateEnum::Stable as u8)));

                    drop(unsafe { Arc::from_raw(read_ptr as *const ManuallyDrop<T>) })
                }
                None => {
                    read_map.remove(&k);
                }
            };
        }

        self.need_disposing.store(false, Ordering::Release);
    }

    fn ref_read(&self) -> &HashMap<Key, (T, AtomicU8)> {
        unsafe { &(*self.read.get()) }
    }

    fn ref_write(&self) -> RwLockReadGuard<'_, HashMap<Key, Option<Arc<T>>>> {
        self.write.read().expect("SyncMap.ref_write# poison guard")
    }
}

impl<Key: Eq + Hash, T> FrameDisposable for SyncMap<Key, T> {
    unsafe fn dispose(&self) {
        self.apply_updates();
        self.need_disposing.store(false, Ordering::Relaxed);
    }
}

#[allow(unused)]
macro_rules! TDD_Describe {
    ($desc:expr $(,$body:expr)?) => {
        {
            let message = format!("{}", $desc);
            println!("• {} {}", format!("{}", &message[0..message.len().min(100)]), format!("{}:{}:0", file!(), line!()));
        }
        $($body)?
    };
}

impl<'a, T> Deref for SyncMapSlot<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            SyncMapSlot::Read(value) => *value,
            SyncMapSlot::Write(value) => &**value,
            SyncMapSlot::Empty => unimplemented!("Empty sync_map slot cannot be dereferenced"),
        }
    }
}

#[test]
fn sync_map() {
    assert_eq!(std::mem::size_of::<SyncMap<String, u32>>(), 128);

    static UPDATED_SYNC_MAP: SyncVec<DisposeItem> = SyncVec::new();

    let map_ptr = Box::into_raw(Box::new(SyncMap::new(Some(addr_of!(UPDATED_SYNC_MAP)))));
    let map = unsafe { &*map_ptr };

    TDD_Describe!("Insert 10 new items from mutliple threads and request disposing once");
    assert_eq!(map.need_disposing.load(Ordering::Acquire), false);
    map.set(format!("v_0"), 0);
    map.set(format!("v_1"), 1);
    let threads = (2..10).rev().into_iter().map(|i| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_nanos(100));
            map.set(format!("v_{i}"), i + 100);
        })
    });
    for t in threads {
        t.join().unwrap()
    }
    assert_eq!(map.ref_read().len(), 0);
    assert_eq!(map.ref_write().len(), 10);

    TDD_Describe!("Apply updates for newly inserted items");
    assert_eq!(map.need_disposing.load(Ordering::Acquire), true);
    assert_eq!(UPDATED_SYNC_MAP.size(), 1);
    unsafe { UPDATED_SYNC_MAP.reset() };
    unsafe { map.apply_updates() };
    assert_eq!(map.need_disposing.load(Ordering::Acquire), false);
    assert_eq!(map.ref_read().len(), 10);
    assert_eq!(map.ref_write().len(), 0);

    TDD_Describe!("Insert 5 new items and update 5 existed items");
    for i in 5..15 {
        map.set(format!("v_{i}"), i + 200);
    }
    assert_eq!(map.ref_read().len(), 10);
    assert_eq!(map.ref_write().len(), 10);

    TDD_Describe!("Delete 2 existed items");
    map.delete(format!("v_0"));
    map.delete(format!("v_0"));
    map.delete(format!("v_1"));
    map.delete(format!("v_1"));
    assert_eq!(map.ref_read().len(), 10);
    assert_eq!(map.ref_write().len(), 12);

    TDD_Describe!("Delete 2 newly created items");
    map.delete(format!("v_13"));
    map.delete(format!("v_13"));
    map.delete(format!("v_14"));
    map.delete(format!("v_14"));
    assert_eq!(map.ref_read().len(), 10);
    assert_eq!(map.ref_write().len(), 10);

    TDD_Describe!("Read non existed and deleted items");
    assert_eq!(map.get(&format!("v_100")), SyncMapSlot::Empty);
    assert_eq!(map.get(&format!("v_0")), SyncMapSlot::Empty);
    assert_eq!(map.get(&format!("v_1")), SyncMapSlot::Empty);
    assert_eq!(map.get(&format!("v_13")), SyncMapSlot::Empty);
    assert_eq!(map.get(&format!("v_14")), SyncMapSlot::Empty);

    TDD_Describe!("Read existed and new items");
    assert_eq!(map.get(&format!("v_2")), SyncMapSlot::Read(&102));
    assert_eq!(map.get(&format!("v_3")), SyncMapSlot::Read(&103));
    assert_eq!(map.get(&format!("v_6")), SyncMapSlot::Write(Arc::new(206)));
    assert_eq!(map.get(&format!("v_7")), SyncMapSlot::Write(Arc::new(207)));
    assert_eq!(map.get(&format!("v_11")), SyncMapSlot::Write(Arc::new(211)));
    assert_eq!(map.get(&format!("v_12")), SyncMapSlot::Write(Arc::new(212)));

    TDD_Describe!("Insert already deleted item");
    map.set(format!("v_0"), 300);
    map.set(format!("v_0"), 300);
    map.set(format!("v_14"), 314);
    map.set(format!("v_14"), 314);
    assert_eq!(map.get(&format!("v_0")), SyncMapSlot::Write(Arc::new(300)));
    assert_eq!(map.get(&format!("v_14")), SyncMapSlot::Write(Arc::new(314)));
    assert_eq!(map.ref_read().len(), 10);
    assert_eq!(map.ref_write().len(), 11);

    TDD_Describe!("Apply deletion & insertion updates");
    assert_eq!(map.need_disposing.load(Ordering::Acquire), true);
    assert_eq!(UPDATED_SYNC_MAP.size(), 1);
    unsafe { UPDATED_SYNC_MAP.reset() };
    unsafe { map.apply_updates() };
    assert_eq!(map.need_disposing.load(Ordering::Acquire), false);
    assert_eq!(map.ref_read().len(), 13);
    assert_eq!(map.ref_write().len(), 0);

    TDD_Describe!("State reset after updates applying");
    assert_eq!(map.get(&format!("v_3")), SyncMapSlot::Read(&103));
    assert_eq!(map.get(&format!("v_6")), SyncMapSlot::Read(&206));
    assert_eq!(map.get(&format!("v_14")), SyncMapSlot::Read(&314));
    for (_, (_, state)) in map.ref_read().iter() {
        assert_eq!(state.load(Ordering::Relaxed), 1);
    }

    let _ = map;
    let _ = unsafe { Box::from_raw(map_ptr) };
}
