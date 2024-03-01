use std::{
    alloc::{dealloc, Layout},
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    sync::atomic::{AtomicPtr, Ordering},
};

// SAFETY: Fields ordering and their types between IVecSnapshot and IVecSnapshotUnsized must be same, then casting between IVecSnapshot<T, 0> -> IVecSnapshotUnsized<T> is safe

#[repr(C)]
struct IVecSnapshot<T, const LEN: usize = 0> {
    len: usize,
    prev: *mut u8,
    data: [T; LEN],
}

#[repr(C)]
struct IVecSnapshotUnsized<T> {
    len: usize,
    prev: *mut u8,
    data: [T],
}

// SAFETY: We can't add non send/sync entries into ivec, that why we don't need to bound T with Send + Sync

pub struct IVec<T> {
    root: AtomicPtr<u8>,
    _marker: PhantomData<T>,
}

pub trait IVecEntry<T: PartialEq + Eq + 'static> {
    fn ivec_id(&self) -> T;
}

// SAFETY: caller must garantee that ptr point to valid IVecSnapshotUnsized instance
unsafe fn read_unsized_ivec<T>(ptr: *const u8) -> *const IVecSnapshotUnsized<T> {
    let len = (*(ptr as *const IVecSnapshot<T>)).len;
    std::ptr::slice_from_raw_parts(ptr as *mut T, len) as *const _
}

// SAFETY: caller must garantee that ptr point to valid IVecSnapshotUnsized instance
unsafe fn read_mut_unsized_ivec<T>(ptr: *mut u8) -> *mut IVecSnapshotUnsized<T> {
    let len = (*(ptr as *const IVecSnapshot<T>)).len;
    std::ptr::slice_from_raw_parts_mut(ptr as *mut T, len) as *mut _
}

fn get_ivec_layout<T>(len: usize) -> Layout {
    let align = std::mem::align_of::<IVecSnapshot<T>>();
    let new_item_size = std::mem::size_of::<T>() * len;
    let fields_size = std::mem::size_of::<IVecSnapshot<T>>();
    let layout_size = fields_size + (new_item_size as f32 / align as f32).ceil() as usize * align;
    Layout::from_size_align(layout_size, align).unwrap()
}

impl<T> IVec<T> {
    pub const fn new() -> Self {
        Self {
            root: AtomicPtr::new(&IVecSnapshot {
                len: 0,
                prev: std::ptr::null_mut(),
                data: [] as [T; 0],
            } as *const _ as *mut _),
            _marker: PhantomData {},
        }
    }

    // SAFETY: You should be very carefully with casting `IVec` to slice
    //         because this allows you to take reference to value in cleared snapshot.
    //         You must garantue by yourself that no-one called clear_prev_snapshots
    pub unsafe fn as_slice(&self) -> &[T] {
        &(*read_unsized_ivec(self.root.load(Ordering::Acquire))).data
    }

    pub fn get_or_insert<TId>(&self, id: TId, init_data: &dyn Fn() -> T) -> T
    where
        TId: Ord + 'static,
        T: Sync + Send + Clone + IVecEntry<TId>,
    {
        // SAFETY: obvious safe, checking that all bits for default state is zero
        assert_eq!(vec![0u8; std::mem::size_of::<IVecSnapshot<T>>()].as_slice(), unsafe {
            std::slice::from_raw_parts(
                &IVecSnapshot { len: 0, data: [] as [T; 0], prev: std::ptr::null_mut() } as *const _
                    as *const u8,
                std::mem::size_of::<IVecSnapshot<T>>(),
            )
        });

        assert_eq!(
            std::mem::size_of::<IVecSnapshot::<T>>()
                + (std::mem::size_of::<[T; 9]>() as f32
                    / std::mem::align_of::<IVecSnapshot<T>>() as f32)
                    .ceil() as usize
                    * std::mem::align_of::<IVecSnapshot<T>>(),
            std::mem::size_of::<IVecSnapshot::<T, 9>>(),
            "IVecSnapshot size is uknown, probably because of changes of rust representation"
        );

        let mut root_ptr = self.root.load(Ordering::Acquire);
        'new_version: loop {
            let root: &IVecSnapshotUnsized<T> = unsafe { &*read_unsized_ivec(root_ptr) };
            if let Ok(result) = root.data.binary_search_by(|value| value.ivec_id().cmp(&id)) {
                return unsafe { root.data.get_unchecked(result).clone() };
            }

            let new_vec_len = root.len + 1;
            let new_vec_u8 = unsafe { std::alloc::alloc_zeroed(get_ivec_layout::<T>(new_vec_len)) };

            // magick hack to create fat pointer
            let new_vec_fat_ptr: *mut [MaybeUninit<T>] =
                std::ptr::slice_from_raw_parts_mut(new_vec_u8 as *mut MaybeUninit<T>, new_vec_len);

            {
                // copy previous data
                unsafe {
                    std::ptr::copy(
                        &root.data as *const _ as *const T,
                        &mut (*(new_vec_fat_ptr as *mut IVecSnapshotUnsized<T>)).data as *mut _
                            as *mut T,
                        root.len,
                    );
                }

                let data = init_data();
                assert!(data.ivec_id() == id);

                // insert new item
                unsafe {
                    std::ptr::write(
                        (&mut (*(new_vec_fat_ptr as *mut IVecSnapshotUnsized<T>)).data as *mut _
                            as *mut T)
                            .add(root.len),
                        data,
                    );
                }
            }

            {
                let new_vec = unsafe { &mut *(new_vec_fat_ptr as *mut IVecSnapshotUnsized<T>) };
                new_vec.len = new_vec_len;
                new_vec.prev = root_ptr;
                new_vec.data.sort_by_key(|v1| v1.ivec_id());
            }

            let root_switch = self.root.compare_exchange(
                root_ptr,
                new_vec_u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            );

            match root_switch {
                Ok(_) => {
                    let new_vec = unsafe { &*(new_vec_fat_ptr as *mut IVecSnapshotUnsized<T>) };
                    break unsafe { new_vec.data.get_unchecked(root.len).clone() };
                }
                Err(new_root_ptr) => {
                    let new_vec = unsafe { &mut *(new_vec_fat_ptr as *mut IVecSnapshotUnsized<T>) };
                    if let Ok(result) =
                        new_vec.data.binary_search_by(|value| value.ivec_id().cmp(&id))
                    {
                        unsafe {
                            (new_vec.data.get_unchecked_mut(result) as *mut T).drop_in_place()
                        };
                        unsafe { dealloc(new_vec_u8, get_ivec_layout::<T>(new_vec_len)) };
                        root_ptr = new_root_ptr;
                        continue 'new_version;
                    } else {
                        unimplemented!("can't happens");
                    }
                }
            };
        }
    }

    unsafe fn _clear_prev_snapshots(&self, root_ptr: *mut u8) -> Option<usize> {
        let &IVecSnapshotUnsized { len, prev, .. } = unsafe { &*read_unsized_ivec::<T>(root_ptr) };
        if len == 0 {
            return None;
        }

        let prev_len = unsafe { (*read_unsized_ivec::<T>(prev)).len };
        if prev_len != 0 {
            unsafe { read_mut_unsized_ivec::<ManuallyDrop<T>>(prev).drop_in_place() }
            unsafe { dealloc(prev, get_ivec_layout::<T>(prev_len)) }
            let ivec = unsafe { &mut *read_mut_unsized_ivec::<T>(root_ptr) };
            ivec.prev = &IVecSnapshot { len: 0, data: [] as [T; 0], prev: std::ptr::null_mut() }
                as *const _ as *mut _;
        }

        Some(len)
    }

    pub unsafe fn clear_prev_snapshots(&self) -> Option<usize> {
        let root_ptr = self.root.load(Ordering::Acquire);
        self._clear_prev_snapshots(root_ptr)
    }
}

impl<T> Drop for IVec<T> {
    fn drop(&mut self) {
        let root_ptr = self.root.load(Ordering::Acquire);
        let clear_op_result = unsafe { self._clear_prev_snapshots(root_ptr) };
        if let Some(ivec_len) = clear_op_result {
            unsafe {
                read_mut_unsized_ivec::<T>(root_ptr).drop_in_place();
                dealloc(root_ptr, get_ivec_layout::<T>(ivec_len));
            }
        }
    }
}

impl<T> Drop for IVecSnapshotUnsized<T> {
    fn drop(&mut self) {
        let prev_len = unsafe { (*read_unsized_ivec::<ManuallyDrop<T>>(self.prev)).len };
        if prev_len != 0 {
            unsafe { read_mut_unsized_ivec::<T>(self.prev).drop_in_place() }
            unsafe { dealloc(self.prev, get_ivec_layout::<T>(prev_len)) }
        }
    }
}

impl IVecEntry<usize> for usize {
    fn ivec_id(&self) -> usize {
        *self
    }
}

impl IVecEntry<u8> for u8 {
    fn ivec_id(&self) -> u8 {
        *self
    }
}

impl<T> IVecEntry<usize> for (usize, T) {
    fn ivec_id(&self) -> usize {
        self.0
    }
}

#[test]
fn ivec_static_upsert_sort() {
    static IVEC: IVec<usize> = IVec::new();
    let expection: Vec<usize> = (100..5000).step_by(123).collect();

    for value in (100..5000).step_by(123).rev() {
        IVEC.get_or_insert(value, &|| value);
        IVEC.get_or_insert(value, &|| value);
    }

    assert_eq!(unsafe { IVEC.as_slice() }, &expection[..]);
}

#[test]
fn ivec_snapshot_clear() {
    let ivec: IVec<usize> = IVec::new();

    let _x = ivec.get_or_insert(4, &|| 4);
    ivec.get_or_insert(2, &|| 2);
    ivec.get_or_insert(1, &|| 1);
    ivec.get_or_insert(3, &|| 3);

    assert!(unsafe { ivec.clear_prev_snapshots().is_some() });
    assert!(unsafe { ivec.clear_prev_snapshots().is_some() });

    // Derefrencing *_x at this point is invalid, but we can't test this
    // you can uncomment following line and run this test with miri
    // println!("{}", *_x);

    assert_eq!(unsafe { ivec.as_slice() }, &[1, 2, 3, 4]);
}

#[test]
fn ivec_custom_align() {
    #[derive(Clone, Copy)]
    #[repr(C, align(16))]
    struct DifferentAlign {
        x: u128,
        y: u128,
    }

    let ivec: IVec<_> = IVec::new();
    ivec.get_or_insert(0, &|| (0, DifferentAlign { x: 0, y: 0 }));
    drop(ivec);

    let ivec: IVec<u8> = IVec::new();
    ivec.get_or_insert(1, &|| 1);
    ivec.get_or_insert(2, &|| 2);
    ivec.get_or_insert(3, &|| 3);
    drop(ivec);
}

#[test]
fn ivec_empty_drop_clear() {
    // * ivec could be dropped without any allocations
    let ivec_empty: IVec<usize> = IVec::new();
    drop(ivec_empty);

    let ivec_empty_cleared: IVec<()> = IVec::new();
    assert!(unsafe { ivec_empty_cleared.clear_prev_snapshots().is_none() });
    drop(ivec_empty_cleared);
}

#[test]
fn ivec_mutli_thread_insert_drop() {
    // * ivec can store ref-counting types
    // * ivec propperly call drop only once for stored values and dealloc all memory
    // * ivec code doesn't cause double memory free
    // * in case of collision ivec propperly drop non inserted value
    use std::sync::Arc;

    let ivec_arc = Arc::new(IVec::new());
    let ivec_box = Arc::new(IVec::new());
    let expection: Vec<_> =
        (100usize..5000).step_by(123).map(|value| (value, Arc::new(value))).collect();

    let threads: Vec<_> = (100..5000)
        .step_by(123)
        .map(|value| {
            let ivec_arc = ivec_arc.clone();
            let ivec_box = ivec_box.clone();

            std::thread::spawn(move || {
                // Thread safe Arc allocations
                ivec_arc.get_or_insert(value, &|| (value, Arc::new(value)));
                ivec_arc.get_or_insert(value, &|| (value, Arc::new(value)));

                // Thread safe Box allocations
                ivec_box.get_or_insert(value, &|| (value, Box::new(value)));
                ivec_box.get_or_insert(value, &|| (value, Box::new(value)));
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    assert_eq!(unsafe { ivec_arc.as_slice() }, &expection[..]);
}
