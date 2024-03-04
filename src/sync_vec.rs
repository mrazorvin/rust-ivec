use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU8, Ordering},
};

const SYNC_VEC_BUCKET_SIZE: usize = 64;

#[repr(C)]
struct SyncVecChunk<T> {
    next: AtomicPtr<SyncVecChunk<T>>,
    raw_len: AtomicU8,
    len: AtomicU8,
    // Relevant only for root node,
    // If there less than 254 chunks, then contains total amount of chunks, else just equals to u8::MAX - 1
    chunks_size: AtomicU8,
    // Relevant only for root node
    // Contains total amount of items for all chunks
    entries_size: AtomicU32,
    values: UnsafeCell<[MaybeUninit<T>; SYNC_VEC_BUCKET_SIZE]>,
}

unsafe impl<T> Send for SyncVecChunk<T> {}
unsafe impl<T> Sync for SyncVecChunk<T> {}

#[repr(transparent)]
struct SyncVec<T> {
    root_chunk: SyncVecChunk<T>,
}

impl<T> SyncVec<T> {
    const fn new() -> Self {
        SyncVec { root_chunk: SyncVecChunk::new() }
    }

    fn size(&self) -> usize {
        self.root_chunk.entries_size.load(Ordering::Acquire) as usize
    }

    fn get(&self, index: usize) -> Option<&T> {
        let size = self.size();

        if index < size {
            Some(unsafe { self.get_unchecked(index) })
        } else {
            None
        }
    }

    fn push(&self, value: T) -> &T {
        let mut chunk_idx = 0;
        let mut chunk = &self.root_chunk;
        let free_index = 'find_free_index_and_chunk: loop {
            let raw_len =
                (SYNC_VEC_BUCKET_SIZE * chunk_idx) + chunk.raw_len.load(Ordering::Acquire) as usize;
            while raw_len >= (SYNC_VEC_BUCKET_SIZE * (chunk_idx + 1)) {
                let mut chunk_ptr = chunk.next.load(Ordering::Acquire);
                if chunk_ptr.is_null() {
                    chunk_ptr = chunk.try_init_next_chunk(&self.root_chunk.chunks_size);
                }
                chunk = unsafe { &*chunk_ptr };
                chunk_idx += 1;
            }
            let next_raw_len = (chunk.raw_len.fetch_add(1, Ordering::Release) + 1);
            if next_raw_len > SYNC_VEC_BUCKET_SIZE as u8 {
                chunk.raw_len.fetch_sub(1, Ordering::Release);
                continue 'find_free_index_and_chunk;
            }
            break 'find_free_index_and_chunk next_raw_len - 1;
        };
        let array_ptr = chunk.values.get() as *mut T;
        let slot_ptr = unsafe { array_ptr.add(free_index as usize) };

        unsafe { slot_ptr.write(value) };
        chunk.len.fetch_add(1, Ordering::AcqRel);
        self.root_chunk.entries_size.fetch_add(1, Ordering::Release);
        unsafe { &*slot_ptr }
    }

    // this function is unsafe because it may returns `u8::MAX` instead of real chunk size
    unsafe fn chunks_size(&self) -> usize {
        self.root_chunk.chunks_size.load(Ordering::Acquire) as usize
    }

    // this function reseat length of all chunks:
    // - when you call this be sure that no-one currently refs vector, while this won't cause
    // - Dropping vec with reset length may not call destructor
    unsafe fn reset(&self) {
        let mut chunk = &self.root_chunk;
        'reset_chunk: loop {
            chunk.raw_len.store(0, Ordering::Release);
            chunk.len.store(0, Ordering::Release);
            let next_ptr = chunk.next.load(Ordering::Acquire);
            if next_ptr.is_null() {
                break 'reset_chunk;
            }
            chunk = &*next_ptr
        }
        self.root_chunk.entries_size.store(0, Ordering::Relaxed);
    }

    unsafe fn get_unchecked(&self, index: usize) -> &T {
        if index < SYNC_VEC_BUCKET_SIZE {
            unsafe {
                return (*self.root_chunk.values.get()).get_unchecked(index).assume_init_ref();
            };
        }

        let mut chunk = &self.root_chunk;
        let mut len = chunk.len.load(Ordering::Acquire) as usize;
        let mut chunk_idx = 0;
        while len >= SYNC_VEC_BUCKET_SIZE && index >= (len + SYNC_VEC_BUCKET_SIZE * chunk_idx) {
            let chunk_ptr = chunk.next.load(Ordering::Acquire);
            chunk = unsafe { &*chunk_ptr };
            chunk_idx += 1;
            len = chunk.raw_len.load(Ordering::Acquire) as usize
        }

        unsafe {
            (*chunk.values.get()).get_unchecked(index % SYNC_VEC_BUCKET_SIZE).assume_init_ref()
        }
    }
}

// impl Iterator {
//
// }

impl<T> Drop for SyncVecChunk<T> {
    fn drop(&mut self) {
        let next_ptr = self.next.load(Ordering::Acquire);

        let values_ptr = self.values.get() as *mut T;
        for i in 0..(self.len.load(Ordering::Acquire) as usize) {
            unsafe { values_ptr.add(i).drop_in_place() }
        }

        if !next_ptr.is_null() {
            unsafe { drop(Box::from_raw(next_ptr)) }
        }
    }
}

impl<T> SyncVecChunk<T> {
    const fn new() -> SyncVecChunk<T> {
        SyncVecChunk {
            next: AtomicPtr::new(std::ptr::null_mut()),
            // the amount of occupied slots, this propery increased every time thread want to init memory
            chunks_size: AtomicU8::new(1),
            raw_len: AtomicU8::new(0),
            entries_size: AtomicU32::new(0),
            // the amount of initiated items, this property increaed only after item fully initiated
            len: AtomicU8::new(0),
            values: unsafe { MaybeUninit::zeroed().assume_init() },
        }
    }

    fn try_init_next_chunk(&self, chunks_size: &AtomicU8) -> *mut SyncVecChunk<T> {
        let next_chunk = Box::into_raw(Box::new(SyncVecChunk::new()));
        let swap_result = self.next.compare_exchange(
            std::ptr::null_mut(),
            next_chunk,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        match swap_result {
            Ok(_) => {
                if chunks_size.load(Ordering::Relaxed) != u8::MAX {
                    let _ = chunks_size.fetch_update(Ordering::Release, Ordering::Acquire, |v| {
                        Some(v.saturating_add(1))
                    });
                }
                next_chunk
            }
            Err(new_next_chunk) => {
                unsafe { drop(Box::from_raw(next_chunk)) }
                new_next_chunk
            }
        }
    }
}

#[test]
fn sync_vec_size() {
    assert_eq!(std::mem::size_of::<SyncVec<()>>(), 16);

    assert_eq!(
        std::mem::size_of::<SyncVec<u8>>(),
        std::mem::size_of::<SyncVec<()>>() + SYNC_VEC_BUCKET_SIZE
    );

    assert_eq!(
        std::mem::size_of::<SyncVec<(u8, u8, u8)>>(),
        std::mem::size_of::<SyncVec<()>>() + SYNC_VEC_BUCKET_SIZE * 3
    );
}

#[test]
fn single_thread_push_and_dealloc() {
    let sync_vec = SyncVec::new();
    for i in 0..SYNC_VEC_BUCKET_SIZE {
        sync_vec.push(Box::new(i));
    }

    assert_eq!(unsafe { sync_vec.chunks_size() }, 1);
    assert_eq!(sync_vec.size(), 64);
}

#[test]
fn multi_thread_push_and_dealloc() {
    use std::sync::{atomic::AtomicUsize, Arc};

    let sync_vec = Arc::new(SyncVec::new());
    static EXPECTED_SIZE: AtomicUsize = AtomicUsize::new(0);
    let threads: Vec<_> = (0..50)
        .map(|v| {
            let sync_vec = Arc::clone(&sync_vec);
            std::thread::spawn(move || {
                for i in 0..v {
                    sync_vec.push(Arc::new(i));
                    EXPECTED_SIZE.fetch_add(1, Ordering::Release);
                }
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap()
    }

    assert_eq!(unsafe { sync_vec.chunks_size() }, sync_vec.size() / SYNC_VEC_BUCKET_SIZE + 1);
    assert_eq!(sync_vec.size(), EXPECTED_SIZE.load(Ordering::Acquire));
}

#[test]
fn reset() {
    let sync_vec = SyncVec::new();
    for i in 0..600 {
        sync_vec.push(i);
    }
    assert_eq!(unsafe { sync_vec.chunks_size() }, 600 / SYNC_VEC_BUCKET_SIZE + 1);

    unsafe { sync_vec.reset() };

    assert_eq!(sync_vec.size(), 0);
    assert_eq!(unsafe { sync_vec.chunks_size() }, 600 / SYNC_VEC_BUCKET_SIZE + 1);

    let mut vec = Vec::new();
    for idx in 0..500 {
        if let Some(value) = sync_vec.get(idx) {
            vec.push(*value);
        }
    }
    assert_eq!(vec.len(), 0);

    for idx in (0..500).rev() {
        sync_vec.push(idx as usize);
    }

    for idx in 0..700 {
        if let Some(value) = sync_vec.get(idx) {
            vec.push(*value);
        }
    }

    assert_eq!(unsafe { sync_vec.chunks_size() }, 600 / SYNC_VEC_BUCKET_SIZE + 1);
    assert_eq!(&(0..500).rev().collect::<Vec<usize>>(), &vec);
}

#[test]
fn get() {
    let sync_vec = SyncVec::new();
    let mut vec = Vec::new();
    for i in 0..500 {
        sync_vec.push(i);
        vec.push(*sync_vec.get(i).unwrap());
    }
    assert_eq!(&vec, &(0..500).collect::<Vec<usize>>());
}
