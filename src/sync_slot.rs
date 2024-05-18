use super::sync_vec::{SyncVec, ZipRangeIterator};
use std::{
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    u32,
};

#[repr(transparent)]
pub struct SyncSlot<const N: usize = 3> {
    index: [u8; N],
}

// SyncSlot must be non copyable and not clonable, and only map can construct them
// because deleting avaialbe onnly with owned value
// mutable access available only with mutable key borrow
// and returned value for shared access has lifetime of passed key
impl<const N: usize> SyncSlot<N> {
    pub const fn invalid() -> Self {
        Self { index: [0; N] }
    }

    // support this method only for N < 4
    fn from_u32(val: u32) -> Self {
        let mut this = Self::invalid();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &val as *const u32 as *const u8,
                &mut this as *mut _ as *mut u8,
                N,
            )
        }
        this
    }

    // support this method only for N < 4
    pub fn as_u32(&self) -> u32 {
        let mut out = 0;
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const Self as *const u8,
                &mut out as *mut u32 as *mut u8,
                N,
            )
        }
        out
    }

    pub fn is_valid(&self) -> bool {
        self.as_u32() != 0
    }

    // support this method only for N < 8
    fn from_usize(val: usize) -> Self {
        let mut this = Self::invalid();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &val as *const usize as *const u8,
                &mut this as *mut Self as *mut u8,
                N,
            )
        }
        this
    }

    // support this method only for N < 8
    fn as_usize(&self) -> usize {
        let mut out = 0;
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const Self as *const u8,
                &mut out as *mut usize as *mut u8,
                N,
            )
        }
        out
    }

    fn as_slot_map_index(&self) -> usize {
        (self.as_usize() - 1) as usize
    }
}

impl<const N: usize> From<SyncSlot<N>> for u32 {
    fn from(value: SyncSlot<N>) -> Self {
        value.as_u32()
    }
}

// to make free access for resource managment we must be able store pointers in slot map, which means that deletion also must take this in account, we also possible want to dirrectly generate id for passed string, which also means that we possible want to use smart string instead of string refs ewherewhere where it possible, also every operration must be lock free and we there should exists simple time tracking with mechanicms

pub struct FreeKeysMeta {
    store_iter: AtomicU32,
    load_iter: AtomicU32,
}

pub struct SlotMeta {
    pub enabled: AtomicBool,
}

pub struct SyncSlotMap<T, const N: usize> {
    free_keys_store_idx: AtomicU32,
    free_keys_meta: [FreeKeysMeta; 2],
    free_keys: [SyncVec<AtomicU32, 512>; 2],
    pub slots: SyncVec<(SlotMeta, MaybeUninit<T>), 1024>,
}

fn get_keys_index(value: u32) -> usize {
    (value % 2) as usize
}

impl<T, const N: usize> SyncSlotMap<T, N> {
    pub const fn new() -> SyncSlotMap<T, N> {
        SyncSlotMap {
            free_keys_store_idx: AtomicU32::new(0),
            free_keys: [SyncVec::new(), SyncVec::new()],
            free_keys_meta: [
                FreeKeysMeta { load_iter: AtomicU32::new(0), store_iter: AtomicU32::new(0) },
                FreeKeysMeta { load_iter: AtomicU32::new(0), store_iter: AtomicU32::new(0) },
            ],
            slots: SyncVec::new(),
        }
    }

    pub fn push(&self, data: T) -> SyncSlot<N> {
        match self.get_free_key() {
            Some(key) => {
                let _ = std::mem::replace(
                    unsafe { &mut *(self.slots.get_unchecked_ptr(key.as_slot_map_index())) },
                    (SlotMeta { enabled: AtomicBool::new(true) }, MaybeUninit::new(data)),
                );
                key
            }
            None => SyncSlot::from_usize(
                self.slots
                    .push((SlotMeta { enabled: AtomicBool::new(true) }, MaybeUninit::new(data)))
                    .1,
            ),
        }
    }

    // SyncSlot is non copyable & clonable, this means that only owner
    // of SyncSlot could delete it, because passing owning value
    // is required that there not exists other shared references
    // it's safe to drop value under taget slot
    pub fn delete(&self, slot: SyncSlot<N>) -> Option<T> {
        if !slot.is_valid() {
            return None;
        }

        let data = unsafe { self.slots.get_unchecked_ptr(slot.as_slot_map_index()) };
        let is_deleted = unsafe { &*data }.0.enabled.compare_exchange(
            true,
            false,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );

        if is_deleted.is_ok() {
            let result = std::mem::replace(unsafe { &mut (&mut *data).1 }, MaybeUninit::uninit());

            let (store_key, store_vec, store_meta) = self.get_store_vec();
            let mut found = false;

            if (store_meta.store_iter.load(Ordering::Acquire) as usize) < store_vec.size() {
                let mut empty_slots = 0;
                let mut iter = ZipRangeIterator::new();
                let mut store_iter = &mut iter.add(
                    &store_vec,
                    store_meta.store_iter.load(Ordering::Acquire) as usize,
                    u32::MAX as usize,
                );

                for mut chunk in iter {
                    let vec = chunk.progress(&mut store_iter);
                    for i in chunk.complete() {
                        let free_key = vec[i].load(Ordering::Acquire);
                        if free_key == 0 {
                            if let Ok(_) = vec[i].compare_exchange(
                                0,
                                slot.as_u32(),
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            ) {
                                found = true;
                                break;
                            } else {
                                empty_slots += 1;
                            }
                        }
                    }
                }

                if empty_slots > 0 && self.get_store_vec().0 == store_key {
                    store_meta.store_iter.fetch_add(empty_slots, Ordering::Release);
                }
            }

            if !found {
                store_vec.push(AtomicU32::new(slot.as_u32()));
            }

            store_meta.store_iter.fetch_add(1, Ordering::Release);

            return unsafe { Some(result.assume_init()) };
        };

        None
    }

    // pub fn get<Key: Into<u32>>(&self, v: Key) -> () {}

    pub fn get_free_key(&self) -> Option<SyncSlot<N>> {
        // the problem that we could change buffer not related to update
        // for example buffer was swapped by another thread
        // thats why we should, be carefully, oor least be sure that changing bad buffer won't cause a lot of problem

        // 1. increasing max by 1 for bad buffer, only take one additional slot, nothig more, so it's probably safe because we just skip one empty slot, even if it's alredy used, in worth keys we could lose single key because there we will interate only up to max
        // 2. increasing current for is worst because we have hihger chnace to skip keys, because we use this as for filtering

        'lookup_free_key: loop {
            let (load_key, load_vec, load_meta) = self.get_load_vec();
            let (store_key, _, store_meta) = self.get_store_vec();

            // load vec is drained
            if load_meta
                .store_iter
                .load(Ordering::Acquire)
                .saturating_sub(load_meta.load_iter.load(Ordering::Acquire))
                == 0
            {
                // no free keys in store vec
                if store_meta.store_iter.load(Ordering::Acquire) == 0 {
                    break None;
                }
                // swap vecs
                else {
                    if let Ok(_) = self.free_keys_store_idx.compare_exchange(
                        load_key,
                        store_key,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    ) {
                        // reset counters to 0
                        store_meta.load_iter.store(0, Ordering::Release);
                        load_meta.store_iter.store(0, Ordering::Release);
                    }

                    // doesn't matter if change was successful or not in both caseses we must repeat process again
                    continue 'lookup_free_key;
                }
            }

            let mut found_free_key: u32 = 0;
            let mut free_slots = 0;
            let mut iter = ZipRangeIterator::new();
            let mut load_iter = &mut iter.add(
                &load_vec,
                load_meta.load_iter.load(Ordering::Acquire) as usize,
                load_meta.store_iter.load(Ordering::Acquire) as usize,
            );

            'linear_lookup: for mut chunk in iter {
                let vec = chunk.progress(&mut load_iter);
                for i in chunk.complete() {
                    let free_key = vec[i].load(Ordering::Acquire);
                    if free_key != 0 {
                        if let Err(_) = vec[i].compare_exchange(
                            free_key,
                            0,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        ) {
                            free_slots += 1
                        } else {
                            found_free_key = free_key;
                            break 'linear_lookup;
                        }
                    }
                }
            }

            let still_same_load = load_key == self.get_load_vec().0;
            if free_slots >= 1 && still_same_load {
                load_meta.load_iter.fetch_add(free_slots, Ordering::Release);
            }

            if found_free_key != 0 {
                load_meta.load_iter.fetch_add(1, Ordering::Release);
                return Some(SyncSlot::from_u32(found_free_key));
            } else if still_same_load {
                return None;
            }
        }
    }

    pub fn get_load_vec(&self) -> (u32, &SyncVec<AtomicU32, 512>, &FreeKeysMeta) {
        let cur_key = self.free_keys_store_idx.load(Ordering::Acquire);
        unsafe {
            (
                cur_key,
                self.free_keys.get_unchecked(get_keys_index(cur_key)),
                self.free_keys_meta.get_unchecked(get_keys_index(cur_key)),
            )
        }
    }

    #[inline]
    pub fn get_store_vec(&self) -> (u32, &SyncVec<AtomicU32, 512>, &FreeKeysMeta) {
        let next_key = self.free_keys_store_idx.load(Ordering::Acquire) + 1;
        unsafe {
            (
                next_key,
                self.free_keys.get_unchecked(get_keys_index(next_key)),
                self.free_keys_meta.get_unchecked(get_keys_index(next_key)),
            )
        }
    }
}

#[test]
fn slot_conversion() {
    assert_eq!(SyncSlot::<3>::from_u32(10).index[0], 10);
    assert_eq!(SyncSlot::<3>::from_u32(10).as_u32(), 10);

    assert_eq!(SyncSlot::<3>::from_usize(10).index[0], 10);
    assert_eq!(SyncSlot::<3>::from_usize(10).as_usize(), 10);
}

#[test]
fn slot_map_async() {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    // The idea behinde this test is to check
    // if all main features works in multi threads as expected
    // to check this every operation is delayed by 1 - 2 millisecond

    // 1. Insert 100 items wtih 2 threads in emtpy slot map
    //    - Syncronization for this part is provided & tested by sync_vec
    //
    // 2. Delete inserted items from 2 threads
    //    - All deleted items must be stored in current keys_store
    //    - Syncronization for this part is also provided & tested by sync_vec
    //
    // 3. Insert following 100 items with 100 threads
    //    - we need to switch current store_vec
    //      with load_vec, and use already existed keys
    //      in result we should re-use all previous deleted keys
    //
    // 4. Delete 50 random keys & Insert 50 new radonm keys
    //    This test continiously delete & insert items at the same time
    //    as result we should contains some delete keys and some new keys
    //    that was created because we run-out of existed keys
    //

    static SLOT_MAP: SyncSlotMap<u32, 3> = SyncSlotMap::new();
    let vec = Arc::new(Mutex::new(HashSet::<u32>::new()));

    let vec1 = Arc::clone(&vec);
    let t1 = std::thread::spawn(move || {
        for i in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            vec1.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    let vec2 = Arc::clone(&vec);
    let t2 = std::thread::spawn(move || {
        for i in 50..100 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            vec2.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();

    let mut expect = vec.lock().unwrap().iter().cloned().collect::<Vec<u32>>();
    expect.sort_by_key(|v| *v);
    assert_eq!(expect, (1..101).into_iter().collect::<Vec<u32>>());
    vec.lock().unwrap().clear();

    let t1 = std::thread::spawn(move || {
        for i in 1..51 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            SLOT_MAP.delete(SyncSlot::from_u32(i));
        }
    });

    let t2 = std::thread::spawn(move || {
        for i in 51..101 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            SLOT_MAP.delete(SyncSlot::from_u32(i));
        }
    });

    assert_eq!(SLOT_MAP.slots.size(), 100);
    t1.join().unwrap();
    t2.join().unwrap();

    let mut new_expect = SLOT_MAP.get_store_vec().1.root_values()[0..100]
        .iter()
        .map(|v| v.load(Ordering::Acquire))
        .collect::<Vec<_>>();
    new_expect.sort_by_key(|v| *v);
    assert_eq!(new_expect, (1..101).into_iter().collect::<Vec<u32>>());

    let vec1 = Arc::clone(&vec);
    let t1 = std::thread::spawn(move || {
        for i in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            vec1.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    let vec2 = Arc::clone(&vec);
    let t2 = std::thread::spawn(move || {
        for i in 50..100 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            vec2.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();
    let mut expect = vec.lock().unwrap().iter().cloned().collect::<Vec<u32>>();
    expect.sort_by_key(|v| *v);
    assert_eq!(expect.len(), 100);
    assert_eq!(expect, (1..101).into_iter().collect::<Vec<u32>>());

    let mut expected = Vec::new();
    for chunk in SLOT_MAP.slots.chunks() {
        for i in 0..chunk.len() {
            if chunk[i].0.enabled.load(Ordering::Relaxed) {
                unsafe { expected.push(chunk[i].1.assume_init()) };
            }
        }
    }
    expected.sort_by_key(|v| *v);
    assert_eq!(expected, (0..100u32).into_iter().collect::<Vec<u32>>());

    vec.lock().unwrap().clear();

    let vec1 = Arc::clone(&vec);
    let t1 = std::thread::spawn(move || {
        for i in 1..51 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            SLOT_MAP.delete(SyncSlot::from_u32(i));
            vec1.lock().unwrap().remove(&unsafe { std::mem::transmute(i as u32) });
        }
    });

    let vec2 = Arc::clone(&vec);
    let t2 = std::thread::spawn(move || {
        for i in 51..101 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            SLOT_MAP.delete(SyncSlot::from_u32(i));
            vec2.lock().unwrap().remove(&unsafe { std::mem::transmute(i as u32) });
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();

    let expect = vec.lock().unwrap().iter().cloned().collect::<Vec<u32>>();
    assert_eq!(expect.len(), 0);

    let vec1 = Arc::clone(&vec);
    let t1 = std::thread::spawn(move || {
        for i in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            vec1.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    let vec2 = Arc::clone(&vec);
    let t2 = std::thread::spawn(move || {
        for i in 50..100 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            vec2.lock().unwrap().insert(SLOT_MAP.push(i).as_u32());
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();
    let mut expect = vec.lock().unwrap().iter().cloned().collect::<Vec<u32>>();
    expect.sort_by_key(|v| *v);

    assert_eq!(expect.len(), 100);
    assert_eq!(expect, (1..101).into_iter().collect::<Vec<u32>>());

    let mut expected: HashSet<u32> = HashSet::new();
    for chunk in SLOT_MAP.slots.chunks() {
        for i in 0..chunk.len() {
            if chunk[i].0.enabled.load(Ordering::Relaxed) {
                expected.insert(unsafe { chunk[i].1.assume_init() });
            }
        }
    }
    assert_eq!(expected, (0..100u32).into_iter().collect::<HashSet<u32>>());
    assert_eq!(
        expect
            .into_iter()
            .map(|v| unsafe { SLOT_MAP.slots.get_unchecked(v as usize).1.assume_init() })
            .collect::<HashSet<u32>>()
            .difference(&expected)
            .cloned()
            .collect::<Vec<u32>>(),
        Vec::<u32>::new(),
    );

    let vec1 = Arc::clone(&vec);
    let t2 = std::thread::spawn(move || {
        for i in 51..101 {
            let lock = vec1.lock();
            std::thread::sleep(std::time::Duration::from_millis(2));
            SLOT_MAP.delete(SyncSlot::from_u32(i));
            lock.unwrap().remove(&unsafe { std::mem::transmute(i as u32) });
        }
    });

    let vec2 = Arc::clone(&vec);
    let t1 = std::thread::spawn(move || {
        for i in 100..150 {
            let lock = vec2.lock();
            std::thread::sleep(std::time::Duration::from_millis(1));
            let slot = SLOT_MAP.push(i).as_u32();
            if !lock.unwrap().insert(slot) {
                panic!("can't insert existed value");
            }
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();
    let expect = vec.lock().unwrap().iter().cloned().collect::<Vec<u32>>();
    assert_eq!(expect.len(), 100);

    let mut contained_items = HashSet::new();
    for chunk in SLOT_MAP.slots.chunks() {
        for i in 0..chunk.len() {
            if chunk[i].0.enabled.load(Ordering::Relaxed) {
                unsafe { contained_items.insert(chunk[i].1.assume_init()) };
            }
        }
    }

    let expection = expect
        .into_iter()
        .map(|v| unsafe { SLOT_MAP.slots.get_unchecked(v as usize - 1).1.assume_init() })
        .collect::<HashSet<u32>>();

    assert_eq!(contained_items.len(), 100);
    assert_eq!(expection.len(), contained_items.len());
    assert_eq!(
        expection.difference(&contained_items).cloned().collect::<Vec<u32>>(),
        Vec::<u32>::new()
    );
}

#[test]
fn slot_map_basics() {
    let slot_map: SyncSlotMap<u32, 3> = SyncSlotMap::new();

    let slot1 = slot_map.push(10);
    assert_eq!(slot1.as_u32(), 1);
    let slot2 = slot_map.push(20);
    assert_eq!(slot2.as_u32(), 2);

    assert_eq!(slot_map.slots.size(), 2);
    assert_eq!(slot_map.delete(slot1), Some(10));
    assert_eq!(slot_map.delete(slot2), Some(20));
    assert_eq!(slot_map.delete(SyncSlot::from_u32(1)), None);
    assert_eq!(slot_map.delete(SyncSlot::from_u32(2)), None);
    assert_eq!(slot_map.delete(SyncSlot::from_u32(3)), None);

    let slot1 = slot_map.push(10);
    assert_eq!(slot1.as_u32(), 1);
    let slot2 = slot_map.push(20);
    assert_eq!(slot2.as_u32(), 2);
    let slot3 = slot_map.push(30);
    assert_eq!(slot3.as_u32(), 3);

    assert_eq!(slot_map.slots.size(), 3);
    assert_eq!(slot_map.delete(slot1), Some(10));
    assert_eq!(slot_map.delete(slot2), Some(20));

    let slot1 = slot_map.push(40);
    assert_eq!(slot_map.delete(slot1), Some(40));
    assert_eq!(slot_map.delete(slot3), Some(30));

    assert_eq!(slot_map.push(20).as_u32(), 2);
    assert_eq!(slot_map.push(10).as_u32(), 1);
    assert_eq!(slot_map.push(30).as_u32(), 3);
    assert_eq!(slot_map.push(40).as_u32(), 4)
}
