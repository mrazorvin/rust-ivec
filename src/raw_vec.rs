use std::mem::ManuallyDrop;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PackedCapacity(pub u8);

impl PackedCapacity {
    pub fn capacity(&self) -> u16 {
        2u16.saturating_pow(self.0 as u32)
    }
}

#[repr(transparent)]
pub struct RawVecU16<T> {
    pub ptr: *mut T,
}

pub fn packed_capacity(len: u16) -> PackedCapacity {
    let packed = PackedCapacity((len as f32).log2().ceil() as u8);
    assert!(packed.0 <= 16);
    packed
}

impl<T> RawVecU16<T> {
    pub fn from_vec(vec: Vec<T>) -> (RawVecU16<T>, PackedCapacity) {
        let mut vec = ManuallyDrop::new(vec);
        let packed_capactity = packed_capacity(vec.capacity() as u16);
        assert_eq!(packed_capactity.capacity(), vec.capacity() as u16);
        (RawVecU16 { ptr: vec.as_mut_ptr() }, packed_capactity)
    }

    // dropping result of this method may cause memory leak
    pub unsafe fn to_vec(self, len: u16, capacity: PackedCapacity) -> Vec<T> {
        Vec::from_raw_parts(self.ptr, len as usize, capacity.capacity() as usize)
    }

    #[inline]
    pub unsafe fn get_unchecked(&self, index: usize) -> &T {
        &*self.ptr.add(index)
    }

    #[inline]
    pub unsafe fn get_unchecked_mut(&mut self, index: usize) -> &mut T {
        &mut *self.ptr.add(index)
    }

    pub fn push(vec: &mut Vec<T>, value: T) -> ((), PackedCapacity) {
        let packed_capacity = packed_capacity((vec.len() + 1).max(2) as u16);
        let expected_capacity = packed_capacity.capacity();
        if vec.len() + 1 > vec.capacity() {
            vec.reserve_exact(expected_capacity as usize - vec.capacity());
            vec.push(value);
        } else {
            vec.push(value);
        }
        assert!(
            expected_capacity == vec.capacity() as u16,
            "reserve_exact reserved more than requested memory"
        );

        ((), packed_capacity)
    }

    /// it's safe to use reserve_exact at least for rust 1.61+
    /// otherwise we must mannualy allocate layout with exact capacity
    /// this could be done by converting vector ptr
    /// alloc.grow(ptr, old_layout, new_layout)
    ///
    /// https://doc.rust-lang.org/std/alloc/fn.dealloc.html
    pub fn resize_with(vec: Vec<T>, new_len: u16, func: fn() -> T) -> (Vec<T>, PackedCapacity) {
        let mut vec = vec;

        if new_len as usize <= vec.len() {
            let current_packed_capacity = packed_capacity(vec.capacity() as u16);
            unsafe { vec.set_len(new_len as usize) };
            return (vec, current_packed_capacity);
        }

        let packed_capacity = packed_capacity(new_len.max(2).max(vec.capacity() as u16));
        let expected_capacity = packed_capacity.capacity();
        if expected_capacity as usize > vec.capacity() {
            vec.reserve_exact(expected_capacity as usize - vec.len());
        }

        if new_len > vec.len() as u16 {
            vec.resize_with(new_len as usize, func);
        }

        assert!(
            expected_capacity == vec.capacity() as u16,
            "reserve_exact {expected_capacity} != {} reserved more than requested memory",
            vec.capacity()
        );

        (vec, packed_capacity)
    }
}

impl<T> Clone for RawVecU16<T> {
    fn clone(&self) -> Self {
        Self { ptr: self.ptr.clone() }
    }
}
