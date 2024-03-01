use std::{
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, AtomicU64, Ordering},
    usize,
};

// Total amount of buckets per single array
// single backet occupied 8bytes of space,
// 128 bucket == 1kb of space
const BUCKETS_PER_ARRAY: usize = 128; // value must be module of 2

// Total amount of items that could be stored by buckets
// which close to the start of array
const BUCKET_DENSITY: usize = 64; // value must be 64 or should be

// Default size of every array should be ~ 1kb
// first 4096 elements is stored in chunks by 64 items
// following 6000k is stored in chunks for 960 items
//
// Maximum amount of items that could be stored in array, must be less than u16::MAX
// with total 128 buckets and START_BUCKET_DENSITY = 64, END_BUCKET_DENSITY, this values
// equals to u16:MAX
const MAX_ITEMS_PER_ARRAY: usize = BUCKETS_PER_ARRAY * BUCKET_DENSITY;

pub fn get_slot_index(id: usize) -> usize {
    id % BUCKET_DENSITY
}

pub fn get_bucket_idx(id: usize) -> usize {
    id / BUCKET_DENSITY
}

pub struct SyncSparseArray<T> {
    pub buckets: [AtomicPtr<Bucket<T>>; BUCKETS_PER_ARRAY],
    pub bucket_bits: [AtomicU64; BUCKETS_PER_ARRAY],
    pub max_id: AtomicU16,
    pub min_id: AtomicU16,
}

pub fn sync_array<T>() -> SyncSparseArray<T> {
    SyncSparseArray {
        buckets: [(); BUCKETS_PER_ARRAY].map(|_| AtomicPtr::new(std::ptr::null_mut())),
        bucket_bits: [(); BUCKETS_PER_ARRAY].map(|_| AtomicU64::new(0)),
        min_id: AtomicU16::new(MAX_ITEMS_PER_ARRAY as u16),
        max_id: AtomicU16::new(0),
    }
}

impl<T> SyncSparseArray<T> {
    /**
     * this function may returns null pointer for bucket that not existed yet
     */
    unsafe fn get_bucket_unchecked(&self, id: usize) -> *mut Bucket<T> {
        assert!(id < MAX_ITEMS_PER_ARRAY);

        let bucket_idx = get_bucket_idx(id);
        let bucket_ptr = self.buckets[bucket_idx].load(Ordering::Relaxed);
        if !bucket_ptr.is_null() {
            return bucket_ptr;
        }

        self.buckets[bucket_idx].load(Ordering::Acquire)
    }

    /**
     * this function returns ptr to existed bucket or create it if needed
     */
    unsafe fn get_bucket_or_create(&self, id: usize) -> *mut Bucket<T> {
        let mut bucket_ptr = unsafe { self.get_bucket_unchecked(id) };

        if bucket_ptr.is_null() {
            bucket_ptr = Box::into_raw(Box::default());
            let swap_result = self.buckets[get_bucket_idx(id)].compare_exchange(
                std::ptr::null_mut(),
                bucket_ptr,
                Ordering::Acquire,
                Ordering::Acquire,
            );
            match swap_result {
                Ok(_) => {}
                Err(current_chunk) => {
                    drop(unsafe { Box::from_raw(bucket_ptr) });
                    bucket_ptr = current_chunk;
                }
            }
        };

        bucket_ptr
    }

    pub fn bits(&self, id: usize) -> u64 {
        self.bucket_bits[get_bucket_idx(id)].load(Ordering::Acquire)
    }

    pub fn min_relaxed(&self) -> u16 {
        self.min_id.load(Ordering::Relaxed)
    }

    pub fn max_relaxed(&self) -> u16 {
        self.max_id.load(Ordering::Relaxed)
    }

    /**
     * Returns bucket lock that safe to mutate in single thread
     *
     * NOTE: if bucket not exists yet, it will be created on demand
     *       don't use this method to check value presence, if
     *       you don't want to empty buckets everywhere
     */
    pub fn bucket_lock(&self, id: usize) -> BucketRefMut<T> {
        let bucket_ptr = unsafe { self.get_bucket_or_create(id) };

        while unsafe { (*bucket_ptr).guard.load(Ordering::Relaxed) } {
            std::hint::spin_loop()
        }

        while unsafe {
            (*bucket_ptr)
                .guard
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        } {}

        BucketRefMut {
            root: self as *const _,
            bucket_ptr,
            bits: &self.bucket_bits[get_bucket_idx(id)] as *const _,
        }
    }

    pub fn bucket(&self, id: usize) -> BucketRef<T> {
        let bucket_ptr = unsafe { self.get_bucket_unchecked(id) };

        BucketRef { bucket_ptr, _bits: self.bucket_bits[get_bucket_idx(id)].as_ptr() }
    }

    pub fn delete_in_place(&self, id: usize) -> Option<T> {
        self.bucket_lock(id).delete(id)
    }

    pub fn set_in_place(&self, id: usize, data: T) -> Option<T> {
        self.bucket_lock(id).set(id, data)
    }
}

impl<T> Drop for SyncSparseArray<T> {
    fn drop(&mut self) {
        for bucket_idx in 0..BUCKETS_PER_ARRAY {
            let bits = self.bucket_bits[bucket_idx].load(Ordering::Acquire);
            if bits != 0 {
                for bit_index in 0..u64::BITS as usize {
                    if (bits & (1 << bit_index)) != 0 {
                        let value = std::mem::replace(
                            unsafe {
                                &mut (*self.buckets[bucket_idx].load(Ordering::Acquire)).slots
                                    [bit_index]
                            },
                            MaybeUninit::uninit(),
                        );
                        unsafe { drop(value.assume_init()) };
                    }
                }
            }

            let bucket_ptr = self.buckets[bucket_idx].load(Ordering::Acquire);
            if !bucket_ptr.is_null() {
                unsafe { self.buckets[bucket_idx].load(Ordering::Acquire).drop_in_place() }
            }
        }
    }
}

pub struct BucketRef<T> {
    _bits: *mut u64,
    pub bucket_ptr: *mut Bucket<T>,
}

impl<T> BucketRef<T> {
    pub unsafe fn get_unchecked(&self, id: usize) -> &T {
        (*(*self.bucket_ptr).slots.get_unchecked(get_slot_index(id))).assume_init_ref()
    }
}

pub struct BucketRefMut<T> {
    root: *const SyncSparseArray<T>,
    bits: *const AtomicU64,
    bucket_ptr: *mut Bucket<T>,
}

impl<T> BucketRefMut<T> {
    pub fn set(&self, id: usize, data: T) -> Option<T> {
        assert!(id < MAX_ITEMS_PER_ARRAY);

        let prev_value = std::mem::replace(
            unsafe { (*self.bucket_ptr).slots.get_unchecked_mut(get_slot_index(id)) },
            MaybeUninit::new(data),
        );

        let is_new_value =
            (unsafe { (*self.bits).load(Ordering::Relaxed) } & (1 << get_slot_index(id))) == 0;
        if is_new_value {
            unsafe { (*self.bits).fetch_or(1 << get_slot_index(id), Ordering::Release) };
            unsafe { (*self.root).min_id.fetch_min(id as u16, Ordering::Acquire) };
            unsafe { (*self.root).max_id.fetch_max(id as u16, Ordering::Acquire) };
            None
        } else {
            unsafe { Some(prev_value.assume_init()) }
        }
    }

    pub fn delete(&mut self, id: usize) -> Option<T> {
        assert!(id < MAX_ITEMS_PER_ARRAY);

        let is_value_exists =
            (unsafe { (*self.bits).load(Ordering::Acquire) } & (1 << get_slot_index(id))) != 0;
        if is_value_exists {
            unsafe { (*self.bits).fetch_and(!(1 << get_slot_index(id)), Ordering::Release) };

            let current_min = unsafe { (*self.root).min_id.load(Ordering::Acquire) } as usize;
            let current_max = unsafe { (*self.root).max_id.load(Ordering::Acquire) } as usize;

            let mut min = MAX_ITEMS_PER_ARRAY;
            let mut max = 0;

            if current_min == id && current_min != current_max {
                'min_search: for bucket_idx in (current_min / 64)..=(current_max / 64) {
                    let bits =
                        unsafe { (*self.root).bucket_bits[bucket_idx].load(Ordering::Acquire) };
                    if bits == 0 {
                        continue;
                    }

                    for bit_index in 0..BUCKET_DENSITY {
                        if (bits & (1 << bit_index)) != 0 {
                            min = min.min(bucket_idx * BUCKET_DENSITY + bit_index);
                            break 'min_search;
                        }
                    }
                }
            }

            if current_max == id && current_min != current_max {
                for bucket_idx in ((current_min / 64)..=(current_max / 64)).rev() {
                    let bits =
                        unsafe { (*self.root).bucket_bits[bucket_idx].load(Ordering::Acquire) };
                    if bits == 0 {
                        continue;
                    }

                    for bit_index in 0..BUCKET_DENSITY {
                        if (bits & (1 << bit_index)) != 0 {
                            max = max.max(bucket_idx * BUCKET_DENSITY + bit_index);
                            break;
                        }
                    }
                }
            }

            if current_min == id {
                #[allow(clippy::redundant_pattern_matching)]
                if let Err(_) = unsafe {
                    (*self.root).min_id.compare_exchange(
                        current_min as u16,
                        min as u16,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                } {
                    unsafe { (*self.root).min_id.fetch_min(min as u16, Ordering::AcqRel) };
                }
            }

            if current_max == id {
                #[allow(clippy::redundant_pattern_matching)]
                if let Err(_) = unsafe {
                    (*self.root).max_id.compare_exchange(
                        current_max as u16,
                        max as u16,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                } {
                    unsafe { (*self.root).max_id.fetch_max(max as u16, Ordering::AcqRel) };
                }
            }

            Some(unsafe {
                std::mem::replace(
                    (*self.bucket_ptr).slots.get_unchecked_mut(get_slot_index(id)),
                    MaybeUninit::uninit(),
                )
                .assume_init()
            })
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn has(&self, id: usize) -> bool {
        unsafe { (*self.bits).load(Ordering::Acquire) & (1 << get_slot_index(id)) != 0 }
    }

    #[allow(dead_code)]
    pub unsafe fn get_unchecked(&self, id: usize) -> &T {
        (*(*self.bucket_ptr).slots.get_unchecked(get_slot_index(id))).assume_init_ref()
    }

    #[allow(dead_code)]
    pub unsafe fn get_mut_unchecked(&mut self, id: usize) -> &mut T {
        (*(*self.bucket_ptr).slots.get_unchecked_mut(get_slot_index(id))).assume_init_mut()
    }

    #[allow(dead_code)]
    pub unsafe fn get_unchecked_copy(&self, id: usize) -> T
    where
        T: Copy,
    {
        (*(*self.bucket_ptr).slots.get_unchecked(get_slot_index(id))).assume_init()
    }

    #[allow(dead_code)]
    pub fn get_copy(&self, id: usize) -> Option<T>
    where
        T: Copy,
    {
        if self.has(id) {
            Some(unsafe {
                (*(*self.bucket_ptr).slots.get_unchecked(get_slot_index(id))).assume_init()
            })
        } else {
            None
        }
    }
}

impl<T> Drop for BucketRefMut<T> {
    fn drop(&mut self) {
        unsafe { (*self.bucket_ptr).guard.store(false, Ordering::Release) };
    }
}

pub struct Bucket<T> {
    guard: AtomicBool,
    pub slots: [MaybeUninit<T>; BUCKET_DENSITY],
}

impl<T> Default for Bucket<T> {
    fn default() -> Self {
        Self {
            guard: AtomicBool::new(false),
            slots: unsafe { MaybeUninit::zeroed().assume_init() },
        }
    }
}

#[test]
fn sync_sparse_array_min_max() {
    let array = sync_array();

    for id in 1..MAX_ITEMS_PER_ARRAY {
        array.set_in_place(id, id);
        assert_eq!(array.min_relaxed(), 1);
        assert_eq!(array.max_relaxed(), id as u16);
    }

    let last_id = MAX_ITEMS_PER_ARRAY - 1;
    assert_eq!(array.max_relaxed(), last_id as u16);

    for id in 1..MAX_ITEMS_PER_ARRAY {
        array.delete_in_place(id);
        assert_eq!(array.min_relaxed(), (id + 1) as u16);
        assert_eq!(array.max_relaxed(), if id == last_id { 0 } else { last_id } as u16);
    }
}

#[test]
fn sync_sparse_array() {
    assert_eq!(std::mem::size_of::<SyncSparseArray<u128>>(), 2056);

    let array: SyncSparseArray<_> = sync_array();

    let insertion_time = std::time::Instant::now();
    let mut bucket_lock = array.bucket_lock(0);
    let mut current_chunk = 0;
    for id in 1..MAX_ITEMS_PER_ARRAY {
        let next_chunk = get_bucket_idx(id);
        if current_chunk != next_chunk {
            drop(bucket_lock);
            bucket_lock = array.bucket_lock(id);
            current_chunk = next_chunk;
        }
        bucket_lock.set(id, id);
    }
    let elapsed = insertion_time.elapsed().as_secs_f32();
    println!("Sync insertion: {}s", elapsed);

    for id in 1..MAX_ITEMS_PER_ARRAY {
        let chunk = unsafe {
            &mut *array.buckets[get_bucket_idx(id)].load(std::sync::atomic::Ordering::Relaxed)
        };

        assert_eq!(id, unsafe { chunk.slots[get_slot_index(id)].assume_init() });
    }

    let insertion_time = std::time::Instant::now();
    let mut current_chunk = 0;
    let mut chunk_index = 1;
    let mut chunk_data = Bucket::default();
    for data in 1..MAX_ITEMS_PER_ARRAY {
        let next_chunk = get_bucket_idx(data);
        if current_chunk != next_chunk {
            array.buckets[current_chunk]
                .store(Box::into_raw(Box::new(chunk_data)), std::sync::atomic::Ordering::Relaxed);

            chunk_data = Bucket::default();

            chunk_index = 0;
            current_chunk = next_chunk;
        }

        chunk_data.slots[chunk_index] = MaybeUninit::new(data);
        chunk_index += 1;
    }
    array.buckets[current_chunk]
        .store(Box::into_raw(Box::new(chunk_data)), std::sync::atomic::Ordering::Relaxed);
    let elapsed = insertion_time.elapsed().as_secs_f32();
    println!("Direct insertion: {}s", elapsed);

    for id in 1..MAX_ITEMS_PER_ARRAY {
        let chunk = unsafe {
            &mut *array.buckets[get_bucket_idx(id)].load(std::sync::atomic::Ordering::Relaxed)
        };

        assert_eq!(id, unsafe { chunk.slots[get_slot_index(id)].assume_init() });
    }

    let insertion_time = std::time::Instant::now();
    for id in 1..MAX_ITEMS_PER_ARRAY {
        array.set_in_place(id, id);
    }
    let elapsed = insertion_time.elapsed().as_secs_f32();
    println!("Set in place: {}s", elapsed);

    for id in 1..MAX_ITEMS_PER_ARRAY {
        let chunk = unsafe {
            &mut *array.buckets[get_bucket_idx(id)].load(std::sync::atomic::Ordering::Relaxed)
        };

        assert_eq!(id, unsafe { chunk.slots[get_slot_index(id)].assume_init() });
    }
}
