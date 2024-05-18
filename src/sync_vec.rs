use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    ops::Index,
    sync::atomic::{AtomicPtr, AtomicU16, AtomicU32, Ordering},
};

#[repr(C)] // 16 bytes
pub struct SyncVecChunk<T, const N: usize> {
    raw_len: AtomicU16,
    len: AtomicU16,
    entries_size: AtomicU32,
    next: AtomicPtr<SyncVecChunk<T, N>>,
    pub values: UnsafeCell<[MaybeUninit<T>; N]>,
}

unsafe impl<T, const N: usize> Send for SyncVecChunk<T, N> {}
unsafe impl<T, const N: usize> Sync for SyncVecChunk<T, N> {}

#[repr(transparent)]
pub struct SyncVec<T, const N: usize = 64> {
    pub root_chunk: SyncVecChunk<T, N>,
    pub _marker: PhantomData<T>,
}

impl<T, const N: usize> SyncVec<T, N> {
    pub const fn new() -> Self {
        SyncVec { root_chunk: unsafe { SyncVecChunk::new() }, _marker: PhantomData {} }
    }

    #[inline]
    pub fn chunk_size(&self) -> usize {
        N
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

    pub fn push(&self, value: T) -> (&T, usize) {
        let mut chunk_idx = 0;
        let mut chunk = &self.root_chunk;
        let free_index = 'find_free_index_and_chunk: loop {
            let raw_len = (N * chunk_idx) + chunk.raw_len.load(Ordering::Acquire) as usize;
            while raw_len >= (N * (chunk_idx + 1)) {
                let mut chunk_ptr = chunk.next.load(Ordering::Acquire);
                if chunk_ptr.is_null() {
                    chunk_ptr = unsafe { chunk.try_init_next_chunk() };
                }
                chunk = unsafe { &*chunk_ptr };
                chunk_idx += 1;
            }
            let next_raw_len = chunk.raw_len.fetch_add(1, Ordering::Release) + 1;
            if next_raw_len > N as u16 {
                chunk.raw_len.fetch_sub(1, Ordering::Release);
                continue 'find_free_index_and_chunk;
            }
            break 'find_free_index_and_chunk next_raw_len - 1;
        };
        let array_ptr = chunk.values.get() as *mut T;
        let slot_ptr = unsafe { array_ptr.add(free_index as usize) };

        unsafe { slot_ptr.write(value) };
        chunk.len.fetch_add(1, Ordering::AcqRel);
        let len = self.root_chunk.entries_size.fetch_add(1, Ordering::Release) as usize;
        unsafe { (&*slot_ptr, len + 1) }
    }

    // iterator over chunks, this function unsafe just to make sure
    // that user understand contract for such iteration
    pub fn chunks(&self) -> ChunksIterator<'_, T, N> {
        ChunksIterator {
            chunk: &self.root_chunk as *const _,
            iterations: self.size(),
            _marker: PhantomData {},
        }
    }

    // this function doesn't return real amount
    pub unsafe fn chunks_size(&self) -> usize {
        ((self.root_chunk.entries_size.load(Ordering::Acquire) as f32) / N as f32).ceil() as usize
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
        unsafe { &*self.get_unchecked_ptr(index) }
    }

    #[inline]
    pub unsafe fn get_unchecked_ptr(&self, index: usize) -> *mut T {
        if index < N {
            unsafe { return (self.root_chunk.values.get() as *mut T).add(index) };
        }

        let mut chunk = &self.root_chunk;
        let mut len = chunk.len.load(Ordering::Acquire) as usize;
        let mut chunk_idx = 0;
        while len >= N && index >= (len + N * chunk_idx) {
            let chunk_ptr = chunk.next.load(Ordering::Acquire);
            chunk = unsafe { &*chunk_ptr };
            chunk_idx += 1;
            len = chunk.raw_len.load(Ordering::Acquire) as usize
        }

        unsafe { (chunk.values.get() as *mut T).add(index % N) }
    }

    pub fn root_values(&self) -> &[T; N] {
        unsafe { &*(self.root_chunk.values.get() as *const [T; N]) }
    }
}

impl<T, const N: usize> Drop for SyncVecChunk<T, N> {
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

impl<T, const N: usize> SyncVecChunk<T, N> {
    const unsafe fn new() -> SyncVecChunk<T, N> {
        SyncVecChunk {
            next: AtomicPtr::new(std::ptr::null_mut()),
            raw_len: AtomicU16::new(0),
            entries_size: AtomicU32::new(0),
            // the amount of initiated items, this property increaed only after item fully initiated
            len: AtomicU16::new(0),
            values: unsafe { MaybeUninit::zeroed().assume_init() },
        }
    }

    #[allow(unused)]
    #[inline]
    unsafe fn get_unchecked(&self, index: usize) -> &T {
        unsafe { &*((self.values.get() as *const T).add(index)) }
    }

    unsafe fn try_init_next_chunk(&self) -> *mut SyncVecChunk<T, N> {
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

pub struct ChunksIterator<'a, T, const N: usize> {
    pub chunk: *const SyncVecChunk<T, N>,
    pub iterations: usize,
    pub _marker: PhantomData<&'a T>,
}

impl<'a, T, const N: usize> Iterator for ChunksIterator<'a, T, N> {
    type Item = ChunkIteratorItem<'a, T, N>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.iterations == 0 {
            return None;
        }

        let len = self.iterations.min(N);
        let chunk = unsafe { &(*self.chunk) };

        self.iterations = self.iterations.saturating_sub(N);
        self.chunk = chunk.next.load(Ordering::Acquire);

        Some(ChunkIteratorItem { chunk, len, idx: 0 })
    }
}

pub struct ChunkIteratorItem<'a, T, const N: usize> {
    pub chunk: &'a SyncVecChunk<T, N>,
    pub idx: usize,
    pub len: usize,
}

impl<'a, T, const N: usize> ChunkIteratorItem<'a, T, N> {
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl<'a, T, const N: usize> Index<usize> for ChunkIteratorItem<'a, T, N> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &'a Self::Output {
        unsafe { &*((self.chunk.values.get() as *const T).add(index)) }
    }
}

#[derive(Debug)]
pub struct ZipRangeIterator<const N: usize> {
    first: bool,
    idx: usize,
    max_idx: usize,
}

impl<const N: usize> ZipRangeIterator<N> {
    pub fn new() -> ZipRangeIterator<N> {
        ZipRangeIterator { idx: 0, max_idx: usize::MAX, first: true }
    }

    pub fn chunk_size(&self) -> usize {
        N
    }

    #[inline]
    pub fn add<'a, T>(
        &mut self,
        sync_vec: &'a SyncVec<T, N>,
        min: usize,
        max: usize,
    ) -> ChunksIterator<'a, T, N> {
        self.max_idx = self
            .max_idx
            .min(sync_vec.root_chunk.entries_size.load(Ordering::Acquire) as usize)
            .min(max);

        self.idx = self.idx.max(min);

        sync_vec.chunks()
    }
}

impl<const N: usize> Iterator for ZipRangeIterator<N> {
    type Item = ZipRangeChunk<N>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.max_idx {
            return None;
        }

        let result = if self.first {
            let end = (self.idx + N).min(self.idx / N * N + N);
            let result = ZipRangeChunk {
                //
                first: true,
                start: self.idx,
                end: end.min(self.max_idx),
            };
            self.idx = end;
            self.first = false;
            result
        } else {
            let result = ZipRangeChunk {
                first: false,
                start: self.idx,
                end: (self.idx + N).min(self.max_idx),
            };
            self.idx = self.idx + N;
            result
        };

        Some(result)
    }
}

pub struct ZipRangeChunk<const N: usize> {
    first: bool,
    start: usize,
    end: usize,
}

impl<const N: usize> ZipRangeChunk<N> {
    pub fn bucket_index(&self) -> usize {
        self.start / N
    }

    #[inline]
    pub fn complete(&self) -> std::ops::Range<usize> {
        (self.start % N)..(self.end - self.start / N * N)
    }

    #[inline]
    pub fn progress<'b, T>(
        &mut self,
        chunks_iterator: &mut ChunksIterator<'b, T, N>,
    ) -> ChunkIteratorItem<'b, T, N> {
        if self.first && self.start / N != 0 {
            for _ in 0..self.start / N {
                chunks_iterator.next();
            }
        }
        chunks_iterator.next().unwrap()
    }
}

#[test]
fn iter() {
    let sync_vec1: SyncVec<_, 64> = SyncVec::new();
    let sync_vec2: SyncVec<_, 64> = SyncVec::new();
    for i in 0..(sync_vec1.chunk_size() * 3) {
        sync_vec1.push(i);
        sync_vec2.push(Box::new(i));
    }

    // single vec iteration
    let mut result_vec = Vec::new();
    for chunk in sync_vec1.chunks() {
        for i in 0..chunk.len() {
            result_vec.push(chunk[i])
        }
    }

    assert_eq!(&result_vec, &(0..sync_vec1.chunk_size() * 3).collect::<Vec<usize>>());

    let mut result_vec: Vec<usize> = Vec::new();

    // #region ### multi-vec range iterator
    let mut range1_iter = ZipRangeIterator::new();

    let chunk1_iter = &mut range1_iter.add(&sync_vec1, 20, sync_vec1.chunk_size() * 3);
    let chunk2_iter = &mut range1_iter.add(&sync_vec2, 30, sync_vec2.chunk_size() * 3 + 10);
    for mut chunk in range1_iter {
        let chunk_1 = chunk.progress(chunk1_iter);
        let chunk_2 = chunk.progress(chunk2_iter);

        for i in chunk.complete() {
            result_vec.push(chunk_1[i] + *chunk_2[i]);
        }
    }

    assert_eq!(
        &result_vec,
        &(30..sync_vec1.chunk_size() * 3).map(|v| v + v).collect::<Vec<usize>>()
    )
}

#[test]
fn sync_vec_size() {
    assert_eq!(std::mem::size_of::<SyncVecChunk<(), 64>>(), 16);
    assert_eq!(std::mem::size_of::<SyncVec<u8>>(), std::mem::size_of::<SyncVec<()>>() + 64);

    assert_eq!(
        std::mem::size_of::<SyncVec<(u8, u8, u8)>>(),
        std::mem::size_of::<SyncVec<()>>() + 64 * 3
    );
}

#[test]
fn single_thread_push_and_dealloc() {
    let sync_vec: SyncVec<_, 64> = SyncVec::new();
    for i in 0..sync_vec.chunk_size() {
        sync_vec.push(Box::new(i));
    }

    assert_eq!(unsafe { sync_vec.chunks_size() }, 1);
    assert_eq!(sync_vec.size(), 64);
}

#[test]
fn multi_thread_push_and_dealloc() {
    use std::sync::{atomic::AtomicUsize, Arc};
    let sync_vec: Arc<SyncVec<Arc<u32>, 64>> = Arc::new(SyncVec::new());
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

    assert_eq!(unsafe { sync_vec.chunks_size() }, sync_vec.size() / sync_vec.chunk_size() + 1);
    assert_eq!(sync_vec.size(), EXPECTED_SIZE.load(Ordering::Acquire));
}

#[test]
fn reset() {
    let sync_vec: SyncVec<_, 64> = SyncVec::new();
    for i in 0..600 {
        sync_vec.push(i);
    }
    assert_eq!(unsafe { sync_vec.chunks_size() }, 600 / sync_vec.chunk_size() + 1);

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

    assert_eq!(unsafe { sync_vec.chunks_size() }, 500 / sync_vec.chunk_size() + 1);
    assert_eq!(&(0..500).rev().collect::<Vec<usize>>(), &vec);
}

#[test]
fn get() {
    let sync_vec: SyncVec<_, 64> = SyncVec::new();
    let mut vec = Vec::new();
    for i in 0..500 {
        sync_vec.push(i);
        vec.push(*sync_vec.get(i).unwrap());
    }
    assert_eq!(&vec, &(0..500).collect::<Vec<usize>>());
}
