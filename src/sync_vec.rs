use core::panic;
use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    num::NonZeroU8,
    ops::Index,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU8, Ordering},
};

const SYNC_VEC_BUCKET_SIZE: usize = 64;

#[repr(C)] // 16 bytes
struct SyncVecChunk<T> {
    _non_zero: NonZeroU8,
    // {field}: u8
    raw_len: AtomicU8,
    len: AtomicU8,
    entries_size: AtomicU32,
    next: AtomicPtr<SyncVecChunk<T>>,
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
        SyncVec { root_chunk: unsafe { SyncVecChunk::new() } }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.root_chunk.entries_size.load(Ordering::Acquire) as usize
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        let size = self.size();

        if index < size {
            Some(unsafe { self.get_unchecked(index) })
        } else {
            None
        }
    }

    pub fn push(&self, value: T) -> &T {
        let mut chunk_idx = 0;
        let mut chunk = &self.root_chunk;
        let free_index = 'find_free_index_and_chunk: loop {
            let raw_len =
                (SYNC_VEC_BUCKET_SIZE * chunk_idx) + chunk.raw_len.load(Ordering::Acquire) as usize;
            while raw_len >= (SYNC_VEC_BUCKET_SIZE * (chunk_idx + 1)) {
                let mut chunk_ptr = chunk.next.load(Ordering::Acquire);
                if chunk_ptr.is_null() {
                    chunk_ptr = unsafe { chunk.try_init_next_chunk() };
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

    // iterator over chunks, this function unsafe just to make sure
    // that user understand contract for such iteration
    pub fn chunks<'a>(&'a self) -> ChunksIterator<'a, T> {
        ChunksIterator {
            chunk: &self.root_chunk as *const _,
            iterations: self.size(),
            _marker: PhantomData {},
        }
    }

    // this function doesn't return real amount
    pub unsafe fn chunks_size(&self) -> usize {
        ((self.root_chunk.entries_size.load(Ordering::Acquire) as f32)
            / SYNC_VEC_BUCKET_SIZE as f32)
            .ceil() as usize
    }

    // this function reseat length of all chunks:
    // - when you call this be sure that no-one currently refs vector, while this won't cause
    // - Dropping vec with reset length may not call destructor
    pub unsafe fn reset(&self) {
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

    #[inline]
    pub unsafe fn get_unchecked(&self, index: usize) -> &T {
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
    const unsafe fn new() -> SyncVecChunk<T> {
        SyncVecChunk {
            _non_zero: NonZeroU8::new_unchecked(1),
            next: AtomicPtr::new(std::ptr::null_mut()),
            raw_len: AtomicU8::new(0),
            entries_size: AtomicU32::new(0),
            // the amount of initiated items, this property increaed only after item fully initiated
            len: AtomicU8::new(0),
            values: unsafe { MaybeUninit::zeroed().assume_init() },
        }
    }

    #[inline]
    unsafe fn get_unchecked(&self, index: usize) -> &T {
        unsafe { &*((self.values.get() as *const T).add(index)) }
    }

    unsafe fn try_init_next_chunk(&self) -> *mut SyncVecChunk<T> {
        let next_chunk = Box::into_raw(Box::new(SyncVecChunk::new()));
        let swap_result = self.next.compare_exchange(
            std::ptr::null_mut(),
            next_chunk,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        match swap_result {
            Ok(_) => next_chunk,
            Err(new_next_chunk) => {
                unsafe { drop(Box::from_raw(next_chunk)) }
                new_next_chunk
            }
        }
    }
}

struct ChunksIterator<'a, T> {
    chunk: *const SyncVecChunk<T>,
    iterations: usize,
    _marker: PhantomData<&'a T>,
}

impl<'a, T> Iterator for ChunksIterator<'a, T> {
    type Item = ChunkIteratorItem<'a, T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.iterations == 0 {
            return None;
        }

        let len = self.iterations.min(SYNC_VEC_BUCKET_SIZE);
        let chunk = unsafe { &(*self.chunk) };

        self.iterations = self.iterations.saturating_sub(SYNC_VEC_BUCKET_SIZE);
        self.chunk = chunk.next.load(Ordering::Acquire);

        Some(ChunkIteratorItem { chunk, len })
    }
}

struct ChunkIteratorItem<'a, T> {
    chunk: &'a SyncVecChunk<T>,
    len: usize,
}

impl<'a, T> ChunkIteratorItem<'a, T> {
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl<'a, T> Index<usize> for ChunkIteratorItem<'a, T> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &'a Self::Output {
        unsafe { &*((self.chunk.values.get() as *const T).add(index)) }
    }
}

#[derive(Debug)]
struct ZipRangeIterator {
    first: bool,
    idx: usize,
    max_idx: usize,
}

impl ZipRangeIterator {
    pub fn new() -> ZipRangeIterator {
        ZipRangeIterator { idx: 0, max_idx: usize::MAX, first: true }
    }

    #[inline]
    pub fn add<'a, T>(
        &mut self,
        sync_vec: &'a SyncVec<T>,
        min: usize,
        max: usize,
    ) -> ChunksIterator<'a, T> {
        self.max_idx = self
            .max_idx
            .min(sync_vec.root_chunk.entries_size.load(Ordering::Acquire) as usize)
            .min(max);
        self.idx = self.idx.max(min);
        sync_vec.chunks()
    }
}

impl Iterator for ZipRangeIterator {
    type Item = ZipRangeChunk;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.max_idx {
            return None;
        }

        let result = ZipRangeChunk {
            first: self.first,
            start: self.idx,
            end: (self.idx + SYNC_VEC_BUCKET_SIZE).min(self.max_idx),
        };

        self.idx += SYNC_VEC_BUCKET_SIZE;
        self.first = false;

        Some(result)
    }
}

struct ZipRangeChunk {
    first: bool,
    start: usize,
    end: usize,
}

impl ZipRangeChunk {
    #[inline]
    pub fn complete(self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    #[inline]
    pub fn progress<'b, T>(
        &mut self,
        chunks_iterator: &mut ChunksIterator<'b, T>,
    ) -> ChunkIteratorItem<'b, T> {
        if self.first && self.start / SYNC_VEC_BUCKET_SIZE != 0 {
            for _ in 0..self.start / SYNC_VEC_BUCKET_SIZE {
                chunks_iterator.next();
            }
        }
        chunks_iterator.next().unwrap()
    }
}

#[test]
fn iter() {
    let sync_vec1 = SyncVec::new();
    let sync_vec2 = SyncVec::new();
    for i in 0..SYNC_VEC_BUCKET_SIZE {
        sync_vec1.push(i);
        sync_vec2.push(i);
    }

    // single vec iteration
    let mut result_vec = Vec::new();
    for chunk in sync_vec1.chunks() {
        for i in 0..chunk.len() {
            result_vec.push(chunk[i])
        }
    }

    assert_eq!(&result_vec, &(0..SYNC_VEC_BUCKET_SIZE).collect::<Vec<usize>>());

    let mut result_vec = Vec::new();

    // #region ### multi-vec range iterator
    let mut range1_iter = ZipRangeIterator::new();

    let chunk1_iter = &mut range1_iter.add(&sync_vec1, 20, SYNC_VEC_BUCKET_SIZE);
    let chunk2_iter = &mut range1_iter.add(&sync_vec2, 30, SYNC_VEC_BUCKET_SIZE + 10);
    for mut chunk in range1_iter {
        let chunk_1 = chunk.progress(chunk1_iter);
        let chunk_2 = chunk.progress(chunk2_iter);

        for i in chunk.complete() {
            result_vec.push(chunk_1[i] + chunk_2[i]);
        }
    }
    // #endregion

    assert_eq!(&result_vec, &(30..SYNC_VEC_BUCKET_SIZE).map(|v| v + v).collect::<Vec<usize>>())
}

#[test]
fn sync_vec_size() {
    assert_eq!(std::mem::size_of::<SyncVecChunk<()>>(), 16);
    assert_eq!(std::mem::size_of::<Option<SyncVec<()>>>(), 16);
    assert_eq!(std::mem::size_of::<Option<ChunkIteratorItem<u128>>>(), 16);

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
    assert_eq!(unsafe { sync_vec.chunks_size() }, 0);

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

    assert_eq!(unsafe { sync_vec.chunks_size() }, 500 / SYNC_VEC_BUCKET_SIZE + 1);
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
