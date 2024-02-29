use std::{
    alloc::{dealloc, Layout},
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    sync::atomic::{AtomicPtr, Ordering},
};

unsafe impl<T, const N: usize> Send for IVecSnapshot<T, N> {}
unsafe impl<T, const N: usize> Sync for IVecSnapshot<T, N> {}

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

struct IVec<T> {
    root: AtomicPtr<u8>,
    _marker: PhantomData<T>,
}

impl<T> Deref for IVec<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        unsafe { &(*read_unsized_ivec(self.root.load(Ordering::Acquire))).data }
    }
}

trait IVecEntry<T: PartialEq + Eq + 'static> {
    fn ivec_id(&self) -> T;
}

fn read_unsized_ivec<T>(ptr: *const u8) -> *const IVecSnapshotUnsized<T> {
    let len = unsafe { &*(ptr as *const IVecSnapshot<T>) }.len;
    std::ptr::slice_from_raw_parts(ptr as *mut T, len) as *const _
}

fn read_mut_unsized_ivec<T>(ptr: *mut u8) -> *mut IVecSnapshotUnsized<T> {
    let len = unsafe { &*(ptr as *const IVecSnapshot<T>) }.len;
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
    const fn new() -> Self {
        Self {
            root: AtomicPtr::new(&IVecSnapshot {
                len: 0,
                prev: std::ptr::null_mut(),
                data: [] as [T; 0],
            } as *const _ as *mut _),
            _marker: PhantomData {},
        }
    }

    fn get_or_insert<TId>(&self, id: TId, init_data: &dyn Fn() -> T) -> &T
    where
        TId: Ord + 'static,
        T: Sync + Send + Clone + IVecEntry<TId>,
    {
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
                return unsafe { root.data.get_unchecked(result) };
            }

            let new_vec_len = root.len + 1;
            let new_vec_u8 = unsafe { std::alloc::alloc_zeroed(get_ivec_layout::<T>(new_vec_len)) };
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
                    break unsafe { new_vec.data.get_unchecked(root.len) };
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

    unsafe fn clear_prev_snapshots(&self, root_ptr: *mut u8) -> bool {
        let ivec = unsafe { &*read_unsized_ivec::<T>(root_ptr) };
        if ivec.len == 0 {
            return false;
        }

        let prev_ivec = read_unsized_ivec::<ManuallyDrop<T>>(ivec.prev);
        let prev_len = unsafe { &*prev_ivec }.len;
        if prev_len != 0 {
            unsafe { read_mut_unsized_ivec::<ManuallyDrop<T>>(ivec.prev).drop_in_place() }
            unsafe { dealloc(ivec.prev, get_ivec_layout::<T>(prev_len)) }
        }
        let ivec = unsafe { &mut *read_mut_unsized_ivec::<T>(root_ptr) };
        ivec.prev = &IVecSnapshot { len: 0, data: [] as [T; 0], prev: std::ptr::null_mut() }
            as *const _ as *mut _;

        true
    }
}

impl<T> Drop for IVec<T> {
    fn drop(&mut self) {
        let root_ptr = self.root.load(Ordering::Acquire);
        let is_prev_snapshots_cleared = unsafe { self.clear_prev_snapshots(root_ptr) };
        if is_prev_snapshots_cleared {
            let ivec_len = unsafe {
                let root_mut = read_mut_unsized_ivec::<T>(root_ptr);
                let len = (*root_mut).len;
                root_mut.drop_in_place();
                len
            };
            unsafe { dealloc(root_ptr, get_ivec_layout::<T>(ivec_len)) }
        }
    }
}

impl<T> Drop for IVecSnapshotUnsized<T> {
    fn drop(&mut self) {
        let prev_ivec = read_unsized_ivec::<ManuallyDrop<T>>(self.prev);
        let prev_len = unsafe { &*prev_ivec }.len;
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

impl<T> IVecEntry<usize> for (usize, T) {
    fn ivec_id(&self) -> usize {
        self.0
    }
}

fn main() {
    let ivec = IVec::new();

    println!("{}", ivec.len());

    ivec.get_or_insert(4, &|| 4);
    ivec.get_or_insert(4, &|| 4);
    ivec.get_or_insert(2, &|| 2);
    ivec.get_or_insert(1, &|| 1);
    ivec.get_or_insert(3, &|| 3);

    println!("{:?}", &ivec[..]);
}

#[test]
fn ivec_upsert_sort() {
    let ivec: IVec<usize> = IVec::new();
    let expection: Vec<usize> = (100..5000).step_by(123).collect();

    for value in (100..5000).step_by(123).rev() {
        ivec.get_or_insert(value, &|| value);
        ivec.get_or_insert(value, &|| value);
    }

    assert_eq!(ivec[..], expection[..]);
}

#[test]
fn ivec_align() {
    #[derive(Clone, Copy)]
    #[repr(C, align(16))]
    struct DifferentAlign {
        x: u128,
        y: u128,
    }

    let ivec_1: IVec<_> = IVec::new();
    ivec_1.get_or_insert(0, &|| (0, DifferentAlign { x: 0, y: 0 }));

    drop(ivec_1);
}

#[test]
fn ivec_empty_drop() {
    // * ivec could be dropped without any allocations

    let ivec_1: IVec<usize> = IVec::new();
    let ivec_2: IVec<usize> = IVec::new();

    drop(ivec_1);
    drop(ivec_2)
}

#[test]
fn ivec_mutli_thread_insert_double_free() {
    // * ivec can store ref-counting types
    // * ivec propperly call drop only once for stored values, but also propperly clear all memory
    // * ivec code doesn't cause double memory free

    use std::sync::Arc;

    let ivec_arc = Arc::new(IVec::new());
    let ivec_box = Arc::new(IVec::new());
    let expection: Vec<_> =
        (100..5000).step_by(123).map(|value| (value, Arc::new(value))).collect();

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

    assert_eq!(ivec_arc[..], expection[..]);
}
