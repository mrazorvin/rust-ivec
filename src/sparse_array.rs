use std::num::NonZeroU16;

use super::{
    primes_u16::PRIME_U16,
    raw_vec::{packed_capacity, PackedCapacity, RawVecU16},
};

pub trait SparseSlot: Default {
    fn get_id(&self) -> NonZeroU16;
}

// ============
// SPARSE ARRAY
// ============
//
// Cons
// ----
// 1. Supports only 0 > {slot_id} < u16::MAX
// 2. Inserting/Deleting values could be slow becacause of hash recalculation
// 3. Slots items must implements `Default` trait
//
// TODO
// ----
// 1. 3 bytes in SparseArray
// 2. Reduce shrinking capatibility with real timer (for example only every 3s)
pub struct SparseArray<T: SparseSlot> {
    hash: u16,
    hash_disabled: bool,

    pub data: RawVecU16<T>, // {data}.len => self.hash_disabled ? self.hash : self.packed_len()
    pub data_capacity: PackedCapacity, // {data}.capacity => 2 ** {data_capacity}
    pub data_slots: u16,    // total non-zero slots
    pub data_slots_end: u16, // pointer to the index after last occupied slot, can point out of vec bound
    pub data_slots_max: u16, // slot with hihger id
    pub data_slots_min: u16, // slot with lower id

    pub bits: RawVecU16<u64>,          // bitfield with occupied slots
    pub bits_capacity: PackedCapacity, // {bits}.capacity => 2 ** {bits_capacity}
}

pub const BITS_SIZE: u16 = u64::BITS as u16;
pub const INVALID_SPARSE_ID: NonZeroU16 = unsafe { NonZeroU16::new_unchecked(u16::MAX) };

/// This value is exlusive i.e real max pack disatance will be == 8
const MAX_COLLISION_PACK_DISTANCE: u16 = 5;

/// Hashing will be disabled if next hash will be above following value.
/// It's mean that looking for next hash may took too much time,
/// ussual there no benefit because vec already contains more than ~10000 items which is around 1/6 of total vec capacity
const MAX_HASH: u16 = 5000;

impl<T: SparseSlot> SparseArray<T> {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.data.ptr
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.data.ptr
    }

    #[inline]
    pub fn capacity(&self) -> u16 {
        packed_capacity(self.hash).capacity()
    }

    #[inline]
    pub fn first_(&self, id: u16) -> &T {
        if id < self.hash {
            return unsafe { self.data.get_unchecked(id as usize) };
        } else {
            return unsafe { self.data.get_unchecked((id % self.hash) as usize) };
        }
    }

    #[inline]
    pub fn first_mut_(&mut self, id: u16) -> &mut T {
        if id < self.hash {
            return unsafe { self.data.get_unchecked_mut(id as usize) };
        } else {
            return unsafe { self.data.get_unchecked_mut((id % self.hash) as usize) };
        }
    }

    #[inline]
    pub unsafe fn prefetch_read(&self, id: u16) {
        if id < self.hash {
            #[cfg(not(miri))]
            std::intrinsics::prefetch_read_data(self.data.ptr.add((id) as usize), 2);
        } else {
            #[cfg(not(miri))]
            std::intrinsics::prefetch_read_data(self.data.ptr.add((id % self.hash) as usize), 2);
        }
    }

    #[inline]
    pub fn memory_info(&self) -> MemoryInfo {
        MemoryInfo {
            used: (self.capacity() as usize) * core::mem::size_of::<T>()
                + std::mem::size_of::<Self>(),
        }
    }
}

pub struct MemoryInfo {
    used: usize,
}

impl<T: SparseSlot> Default for SparseArray<T> {
    fn default() -> Self {
        let (data, data_capacity) = RawVecU16::from_vec(vec![Default::default()]);
        let (bits, bits_capacity) = RawVecU16::from_vec(vec![0]);

        SparseArray {
            hash: 1,
            hash_disabled: false,

            data,
            data_capacity,
            data_slots: 0,
            data_slots_end: 1,
            data_slots_min: 0,
            data_slots_max: 0,

            bits,
            bits_capacity,
        }
    }
}

const INVALID_SLOT_ID: u16 = u16::MAX;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SetOpResult {
    InvalidId = 0,
    UpdateFast = 1,
    UpdatePacked = 2,

    UpsertHashDisabled = 3,

    Insert = 4,
    InsertPacked = 5,
    InsertRehash = 6,
}

// macro is used instead of function, because function can't express that it borrow only specific field
macro_rules! bits {
    (@set $self:ident, $request_id:ident) => {
        let new_chunks_size = $request_id / super::sparse_array::BITS_SIZE + 1;
        if new_chunks_size > $self.bits_capacity.capacity() {
            let bits_vec = unsafe { $self.bits.clone().to_vec(bits!(@len $self), $self.bits_capacity) };
            let (bits, bits_capacity) = RawVecU16::from_vec(RawVecU16::resize_with(bits_vec, packed_capacity(new_chunks_size).capacity(), Default::default).0);
            $self.bits = bits;
            $self.bits_capacity = bits_capacity;
        }
        let bits = unsafe { $self.bits.get_unchecked_mut(($request_id / super::sparse_array::BITS_SIZE) as usize) };
        *bits = *bits | (1 << ($request_id % 64));
        $self.data_slots_max = $self.data_slots_max.max($request_id);

        // by default {data_slots_min = 0}. 0 slot is used only for optimizing
        // fetch op for modulus hashing, because of that 0 removed from comparison
        if $self.data_slots_min == 0 || $self.data_slots_min > $request_id {
            $self.data_slots_min = $request_id
        }
    };
    (@len $self:ident) => {
        $self.bits_capacity.capacity()
    };
}

impl<T: SparseSlot> SparseArray<T> {
    pub fn data_hash_len(&self) -> u16 {
        return if self.data_slots == 0 { 1 } else { self.hash + MAX_COLLISION_PACK_DISTANCE };
    }

    pub fn data_hash_disabled_len(&self) -> u16 {
        self.hash
    }

    pub fn set(&mut self, data: T) -> SetOpResult {
        if data.get_id().get() == INVALID_SLOT_ID {
            return SetOpResult::InvalidId;
        }

        let requested_id = data.get_id().get();
        let requested_slot_index = requested_id % self.hash;
        let slot = unsafe { self.data.get_unchecked_mut(requested_slot_index as usize) };
        if slot.get_id() == data.get_id() {
            let _ = std::mem::replace(slot, data);

            return SetOpResult::UpdateFast;
        }

        let requested_id = data.get_id().get();
        if self.hash_disabled {
            if requested_id >= self.hash {
                let data_vec = unsafe { self.data.clone().to_vec(self.hash, self.data_capacity) };
                self.hash = 2u16.saturating_pow((packed_capacity(requested_id).0 + 1) as u32);
                let (data, data_capacity) = RawVecU16::from_vec(
                    RawVecU16::resize_with(
                        data_vec,
                        self.data_hash_disabled_len(),
                        Default::default,
                    )
                    .0,
                );
                self.data = data;
                self.data_capacity = data_capacity;
            }

            if requested_id >= self.data_slots_end {
                self.data_slots_end = requested_id + 1;
            }

            let slot = unsafe { self.data.get_unchecked_mut(requested_id as usize) };
            let is_new_item = slot.get_id().get() == u16::MAX;
            if is_new_item {
                self.data_slots += 1;
                bits!(@set self, requested_id);
            }

            *slot = data;

            return SetOpResult::UpsertHashDisabled;
        }

        let is_valid_free_slot = slot.get_id().get() == u16::MAX && requested_slot_index != 0;
        if is_valid_free_slot {
            self.data_slots_end = self.data_slots_end.max(requested_slot_index + 1);
            self.data_slots += 1;
            bits!(@set self, requested_id);

            *slot = data;

            return SetOpResult::Insert;
        }

        let mut free_slot_index = INVALID_SLOT_ID;
        for i in ((requested_slot_index as usize) + 1)
            ..((requested_slot_index + MAX_COLLISION_PACK_DISTANCE) as usize)
                .min(self.data_hash_len() as usize)
        {
            let slot = unsafe { self.data.get_unchecked_mut(i) };
            if slot.get_id() == data.get_id() {
                *slot = data;

                return SetOpResult::UpdatePacked;
            }

            if slot.get_id().get() == INVALID_SLOT_ID && free_slot_index == INVALID_SLOT_ID {
                // FIXME it's possible that we could break there, since after deletion we keep entities as packed as possible and there only free/invalid slots after first occurency
                free_slot_index = i as u16;
            }
        }

        if free_slot_index != INVALID_SLOT_ID {
            let mut insertion_data = data;
            if insertion_data.get_id().get() < self.hash {
                // FIXME needed a test
                insertion_data = std::mem::replace(
                    unsafe { self.data.get_unchecked_mut(requested_slot_index as usize) },
                    insertion_data,
                );
            }
            let slot = unsafe { self.data.get_unchecked_mut(free_slot_index as usize) };
            *slot = insertion_data;

            self.data_slots_end = self.data_slots_end.max(free_slot_index + 1);
            self.data_slots += 1;
            bits!(@set self, requested_id);

            return SetOpResult::InsertPacked;
        }

        let mut data_vec =
            unsafe { self.data.clone().to_vec(self.data_hash_len(), self.data_capacity) };

        if self.data_slots_end < self.data_hash_len() {
            data_vec[self.data_slots_end as usize] = data;
        } else {
            RawVecU16::push(&mut data_vec, data);
        }

        self.data_slots_end += 1;
        self.data_slots += 1;
        bits!(@set self, requested_id);

        self.rehash_data(self.hash, data_vec);

        return SetOpResult::InsertRehash;
    }

    fn rehash_data(&mut self, new_hash: u16, mut data_vec: Vec<T>) {
        let mut collisions_info;
        let mut new_hash = new_hash;
        let shrinking = new_hash < self.hash;

        'hash_lookup: loop {
            new_hash = PRIME_U16
                .iter()
                .find(|prime| {
                    **prime >= (new_hash + (new_hash / 10).max(1).min(200))
                        && (if !shrinking { **prime >= self.data_slots_end } else { true })
                })
                .map(|prime| *prime)
                .unwrap_or(new_hash + 1);

            if new_hash > MAX_HASH {
                new_hash = data_vec
                    .iter()
                    .filter(|x| x.get_id().get() != u16::MAX)
                    .fold(u16::MIN, |hash, slot| hash.max(slot.get_id().get()))
                    .max(MAX_HASH)
                    + 1;

                // the lenght of {collisions_info} is very important, for sparse form it always must equals to hash
                collisions_info = vec![0u16; new_hash as usize];
                self.hash_disabled = true;
            } else {
                // the lenght of {collisions_info} is very important, for packed form it always must equals to hash + MAX_COLLISION_PACK_DISTANCE
                collisions_info = vec![0u16; (new_hash + MAX_COLLISION_PACK_DISTANCE) as usize];
            }

            // FIXME delete debug comments
            //
            // if new_hash == 59 {
            //     for i in 0..data_vec.len() as usize {
            //         println!("{:?} <- {}", &data_vec[i].index(), i);
            //     }
            //     println!("{}, {}", self.data_len, self.len);
            // }

            'collect_collision_info: for index in 0..self.data_slots_end as usize {
                let slot = &data_vec[index];

                let skip_empty_slot = slot.get_id().get() == INVALID_SLOT_ID;
                if skip_empty_slot {
                    continue 'collect_collision_info; // skip EMPTY slots
                }

                let new_slot_index = slot.get_id().get() % new_hash;

                let invalid_modulus_zero_hash = new_slot_index == 0;
                if invalid_modulus_zero_hash {
                    continue 'hash_lookup;
                }

                let index_in_collision_info =
                    unsafe { collisions_info.get_unchecked_mut(new_slot_index as usize) };
                if *index_in_collision_info == 0 {
                    *index_in_collision_info = slot.get_id().get();
                } else {
                    let mut packing_id = slot.get_id().get();
                    // 1. for faster re-hashing all id's which lesser than new_hash
                    // always placed in the same index
                    //
                    // 2. for perfomant read-write operations non-packed
                    // id's have higher priority
                    if slot.get_id().get() < new_hash {
                        packing_id = *index_in_collision_info;
                        *index_in_collision_info = slot.get_id().get();
                    } else if *index_in_collision_info % new_hash != new_slot_index {
                        packing_id = *index_in_collision_info;
                        *index_in_collision_info = slot.get_id().get();
                    }

                    let max_pack_distance =
                        (packing_id % new_hash + MAX_COLLISION_PACK_DISTANCE) as usize;
                    'packing_iter: for pack_index in
                        ((packing_id % new_hash) as usize + 1)..max_pack_distance
                    {
                        let collision_info =
                            unsafe { collisions_info.get_unchecked_mut(pack_index) };

                        if *collision_info == 0 {
                            *collision_info = packing_id;
                            break 'packing_iter;
                        }

                        if pack_index == (max_pack_distance - 1) {
                            let mut skip_len = MAX_COLLISION_PACK_DISTANCE;
                            'hash_pack_offset: for pack_index_hash_index_offset in
                                pack_index..collisions_info.len()
                            {
                                if unsafe {
                                    *collisions_info.get_unchecked(pack_index_hash_index_offset)
                                } != 0
                                {
                                    break 'hash_pack_offset;
                                }
                                skip_len += 1;
                            }
                            new_hash += skip_len;

                            continue 'hash_lookup;
                        }
                    }
                }
            }

            break 'hash_lookup;
        }

        // if new_hash == 137 {
        //     dbg!(self.data_len, &data_vec.len(), &data_vec.capacity());
        //     println!("{:?}", &data_vec);
        // }

        if data_vec.len() < collisions_info.len() {
            data_vec =
                RawVecU16::resize_with(data_vec, collisions_info.len() as u16, Default::default).0;
        }

        //
        //         if new_hash == 137 {
        //             dbg!(new_hash, &collisions_info.len(), self.data_len);
        //             println!("{:?}", &collisions_info);
        //         }

        // if new_hash == 137 {
        //     println!("");
        //     println!("{:?}", data_vec.iter().map(|x| x.index()).collect::<Vec<u16>>());
        // }

        // take number
        // if id < new_hash && new_index == old_index leave number as it is
        // find value in collision_info

        // println!("{collisions_info:?} new_hash: {new_hash}");
        // println!(
        //     "{:?} last_index: {}",
        //     data_vec.iter().map(|x| x.get_id().get()).collect::<Vec<u16>>(),
        //     self.data_last_index
        // );

        let mut data_len = 1;
        for i in 1..self.data_slots_end as usize {
            'swap_index: loop {
                let id = data_vec[i].get_id().get();
                let new_index = id % new_hash;
                if id == INVALID_SLOT_ID {
                    break 'swap_index;
                }

                if id < new_hash && collisions_info[i] == id {
                    // if id < new_hash && new_index == old_index {
                    data_len = data_len.max(i);
                    break 'swap_index;
                }

                let new_index = new_index as usize;
                let collision_id = collisions_info[new_index];

                if collision_id == id {
                    if i != new_index {
                        data_vec.swap(i, new_index);
                        data_len = data_len.max(new_index);
                        continue 'swap_index;
                    } else {
                        data_len = data_len.max(i);
                        break 'swap_index;
                    }
                }

                for pack_index in new_index + 1..new_index + MAX_COLLISION_PACK_DISTANCE as usize {
                    if collisions_info[pack_index] == id {
                        if pack_index != i {
                            data_len = data_len.max(pack_index);
                            data_vec.swap(i, pack_index);
                            continue 'swap_index;
                        } else {
                            data_len = data_len.max(i);
                            break 'swap_index;
                        }
                    }
                }

                // dbg!(new_hash);
                // if i == 33 && new_index == 14 && new_hash == 59 {
                //     println!("{:?}", &collisions_info);
                // }
                // println!(
                //     "LOOP {i} <-> {new_index} | {} <-> {} | {} <-> {} [new_hash: {new_hash}]",
                //     collisions_info[i],
                //     collisions_info[new_index],
                //     data_vec[i].index(),
                //     data_vec[new_index].index(),
                // );
                // if i == 26 && new_index == 107 && new_hash == 137 {
                //     dbg!(data_vec.len(), data_vec.capacity());
                //     println!("{:?}", data_vec.iter().map(|x| x.index()).collect::<Vec<u16>>());
                //     panic!("{:?}", &collisions_info);
                // }
            }
        }

        // shrink data_vec, this must be done after re-hasing
        if data_vec.len() > collisions_info.len() {
            data_vec =
                RawVecU16::resize_with(data_vec, collisions_info.len() as u16, Default::default).0;
        }

        // if (self.data_len > 10000) {
        //     println!(
        //         "{} {} LEN - AAAAA {} {:?}",
        //         self.data_len,
        //         self.hash,
        //         collisions_info.iter().filter(|x| **x > 0).count(),
        //         collisions_info
        //             .into_iter()
        //             .map(|val| (val, val as u16 % self.hash))
        //             .collect::<Vec<(u32, u16)>>(),
        //     );
        //     panic!();
        // }

        // if (data_len + 1) == 2 || new_hash == 2 {
        //     println!(
        //         "{:?} {} {:?} {}",
        //         collisions_info,
        //         self.data_last_index,
        //         data_vec.iter().map(|x| x.get_id().get()).collect::<Vec<u16>>(),
        //         self.hash
        //     );
        //     // println!("WTF {}", self.data_last_index);
        // }

        self.data_slots_end = (data_len + 1) as u16;
        let (data, data_capacity) = RawVecU16::from_vec(data_vec);
        self.data = data;
        self.data_capacity = data_capacity;
        self.hash = new_hash;
    }

    pub fn delete(&mut self, id: NonZeroU16) -> Option<T> {
        let requested_id = id.get();
        let slot_index = id.get() % self.hash;
        let slot = unsafe { self.data.get_unchecked_mut(slot_index as usize) };

        if id.get() == INVALID_SLOT_ID || slot.get_id().get() == INVALID_SLOT_ID {
            return None;
        }
        let mut result: Option<T> = None;
        let mut removed_index = slot_index;
        if slot.get_id().get() == id.get() {
            result = Some(std::mem::replace(slot, Default::default()));
        } else {
            let mut target_found = false;
            for i in ((slot_index as usize) + 1)
                ..((slot_index + MAX_COLLISION_PACK_DISTANCE) as usize)
                    .min(self.data_hash_len() as usize)
            {
                let slot = unsafe { self.data.get_unchecked_mut(i) };
                if slot.get_id() == id {
                    result = Some(std::mem::replace(slot, Default::default()));

                    removed_index = i as u16;
                    target_found = true;
                    break;
                }
            }

            if !target_found {
                return None;
            }
        }

        // we can't decrement data_bits_chunks_size, because then :()
        // we can't re-hash because current array capaciity depends on hash :()

        self.data_slots -= 1;
        self.delete_bit(requested_id);

        if removed_index == (self.data_slots_end - 1) {
            for i in (0..self.data_slots_end - 1).rev() {
                let slot = unsafe { self.data.get_unchecked_mut(i as usize) };
                if slot.get_id().get() != u16::MAX {
                    self.data_slots_end = i + 1;
                    break;
                }
            }
        }

        if self.data_slots == 0 {
            self.data_slots_end = 1;
        }

        if removed_index == slot_index && !self.hash_disabled {
            for i in ((slot_index as usize) + 1)
                ..((slot_index + MAX_COLLISION_PACK_DISTANCE) as usize)
                    .min(self.data_hash_len() as usize)
            {
                if (unsafe { self.data.get_unchecked_mut(i) }.get_id().get() % self.hash)
                    == slot_index
                {
                    let origin_slot = unsafe { &mut *self.data.ptr.add(slot_index as usize) };
                    let new_slot = unsafe { &mut *self.data.ptr.add(i) };
                    std::mem::swap(origin_slot, new_slot);
                    break;
                }
            }
        }

        self.try_shrink();

        return result;
    }

    #[inline]
    pub fn get_(&self, id: u16) -> &T {
        if id < self.hash {
            return unsafe { self.data.get_unchecked(id as usize) };
        }

        if id == u16::MAX {
            return unsafe { self.data.get_unchecked(0) };
        }

        let slot_index = id % self.hash;
        for i in slot_index..(slot_index + MAX_COLLISION_PACK_DISTANCE).min(self.data_hash_len()) {
            let slot = unsafe { self.data.get_unchecked(i as usize) };
            if slot.get_id().get() == id {
                return slot;
            }
        }

        unsafe { self.data.get_unchecked(0) }
    }

    pub fn get_mut_(&mut self, id: u16) -> &mut T {
        if id < self.hash {
            return unsafe { self.data.get_unchecked_mut(id as usize) };
        }

        if id == u16::MAX {
            return unsafe { self.data.get_unchecked_mut(0) };
        }

        let slot_index = id % self.hash;
        for i in slot_index..(slot_index + MAX_COLLISION_PACK_DISTANCE).min(self.data_hash_len()) {
            if unsafe { self.data.get_unchecked(i as usize) }.get_id().get() == id {
                return unsafe { self.data.get_unchecked_mut(i as usize) };
            }
        }

        unsafe { self.data.get_unchecked_mut(0) }
    }

    fn delete_bit(&mut self, requested_id: u16) {
        let bits = unsafe { self.bits.get_unchecked_mut((requested_id / BITS_SIZE) as usize) };
        *bits = *bits & !(1 << (requested_id % 64));

        if self.data_slots != 0 {
            if requested_id == self.data_slots_max {
                'max_lookup: for i in
                    ((self.data_slots_min / BITS_SIZE)..=(self.data_slots_max / BITS_SIZE)).rev()
                {
                    let bits = unsafe { self.bits.get_unchecked_mut(i as usize) };
                    if *bits != 0 {
                        for bit_index in (0..BITS_SIZE).rev() {
                            if (*bits & (1 << bit_index)) != 0 {
                                self.data_slots_max = i * BITS_SIZE + bit_index;
                                break 'max_lookup;
                            }
                        }
                    }
                }
            }

            if requested_id == self.data_slots_min {
                'min_lookup: for i in
                    (self.data_slots_min / BITS_SIZE)..=(self.data_slots_max / BITS_SIZE)
                {
                    let bits = unsafe { self.bits.get_unchecked_mut(i as usize) };
                    if *bits != 0 {
                        for bit_index in 0..BITS_SIZE {
                            if (*bits & (1 << bit_index)) != 0 {
                                self.data_slots_min = i * BITS_SIZE + bit_index;
                                break 'min_lookup;
                            }
                        }
                    }
                }
            }
        } else {
            self.data_slots_min = 0;
            self.data_slots_max = 0;
        }
    }

    pub fn try_shrink(&mut self) {
        if self.data_slots == 0 {
            return;
        }

        let expected_hash;

        // 150 & 50 / 4.0 is 10.0 is empiric values just works good during tests
        if self.hash > 150 && (self.hash as f32 / self.data_slots as f32) > 4.0 {
            expected_hash = self.data_slots.saturating_mul(2);
        } else if self.hash > 50 && (self.hash as f32 / self.data_slots as f32) > 10.0 {
            expected_hash = self.data_slots.saturating_mul(2);
        } else {
            return;
        }

        if expected_hash < MAX_HASH {
            let data_vec = unsafe {
                self.data.clone().to_vec(
                    if self.hash_disabled {
                        self.data_hash_disabled_len()
                    } else {
                        self.data_hash_len()
                    },
                    self.data_capacity,
                )
            };

            self.hash_disabled = false;
            self.rehash_data(expected_hash, data_vec);
        }
    }
}

impl<T: SparseSlot> Drop for SparseArray<T> {
    fn drop(&mut self) {
        if self.hash_disabled {
            let data_vec = unsafe {
                self.data.clone().to_vec(self.data_hash_disabled_len(), self.data_capacity)
            };
            let bits_vec =
                unsafe { self.bits.clone().to_vec(bits!(@len self), self.bits_capacity) };
            drop(data_vec);
            drop(bits_vec);
        } else {
            let data_vec =
                unsafe { self.data.clone().to_vec(self.data_hash_len(), self.data_capacity) };
            let bits_vec =
                unsafe { self.bits.clone().to_vec(bits!(@len self), self.bits_capacity) };
            drop(data_vec);
            drop(bits_vec);
        }
    }
}

/// <- 16 archetype -> <- 16 index ->
///
/// EntityId(u64::MAX) -> non exist entity
/// EntityId(0)        -> invalid / non exists entity
///
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(C)]
struct EntityId {
    id: NonZeroU16,
    arch: NonZeroU16,
}

#[cfg(test)]
impl EntityId {
    pub fn id(&self) -> NonZeroU16 {
        self.id
    }

    pub fn arch(&self) -> NonZeroU16 {
        self.arch
    }
}

#[cfg(test)]
impl Default for EntityId {
    fn default() -> Self {
        EntityId {
            arch: unsafe { NonZeroU16::new_unchecked(u16::MAX) },
            id: unsafe { NonZeroU16::new_unchecked(u16::MAX) },
        }
    }
}

#[cfg(test)]
impl SparseSlot for EntityId {
    fn get_id(&self) -> NonZeroU16 {
        self.id()
    }
}

#[cfg(test)]
impl SparseArray<EntityId> {
    pub fn get(&self, entity: EntityId) -> &EntityId {
        self.get_(entity.id().get())
    }

    pub fn get_mut(&mut self, id: NonZeroU16) -> &mut EntityId {
        self.first_mut_(id.get())
    }
}

#[cfg(test)]
mod entity_id {
    use super::EntityId;

    pub fn invalid_new(raw_id: u16) -> EntityId {
        assert!(raw_id != 0);

        EntityId {
            id: unsafe { std::num::NonZeroU16::new_unchecked(raw_id) },
            arch: unsafe { std::num::NonZeroU16::new_unchecked(u16::MAX) },
        }
    }

    pub fn new(raw_id: u16, raw_arch: u16) -> EntityId {
        assert!(raw_id != 0 && raw_id != u16::MAX);
        assert!(raw_arch != 0 && raw_arch != u16::MAX);

        EntityId {
            id: unsafe { std::num::NonZeroU16::new_unchecked(raw_id) },
            arch: unsafe { std::num::NonZeroU16::new_unchecked(raw_arch) },
        }
    }
}

#[test]
fn zero_entities() {
    // get_first* methods is the fastest way to get value from array
    // since they ignore bound check and returns only first occurency
    //
    // because of that result of this operation must be always
    // checked in user code
    //
    // this test check that:
    //
    // * empty arrays initiated with valid state
    // * we may call get_first even on empty array

    let array: SparseArray<EntityId> = SparseArray::default();
    assert_eq!(array.hash, 1);
    assert_eq!(array.hash_disabled, false);

    assert_eq!(array.data_slots, 0);
    assert_eq!(array.data_slots_end, 1);
    assert_eq!(array.data_slots_max, 0);
    assert_eq!(array.data_slots_min, 0);
    assert_eq!(array.bits_capacity.capacity(), 1);
    assert_eq!(array.data_capacity.capacity(), 1);

    assert_eq!(array.get_(0).id().get(), INVALID_SLOT_ID);
    assert_eq!(array.get_(1).id().get(), INVALID_SLOT_ID);
    assert_eq!(array.get_(2).id().get(), INVALID_SLOT_ID);
    assert_eq!(array.get_(42).id().get(), INVALID_SLOT_ID);
    assert_eq!(array.get_(43).id().get(), INVALID_SLOT_ID);
}

#[test]
fn inserting_and_reading_entities() {
    // set method automatically increase size of the array and recalculate hash if needed
    //
    // this test check that:
    //
    // * user could add multiple values into array
    // * conflicting id's select re-hashing branch, change array length and set valid value
    // * non-conflicting id's update exited slot without re-hashing
    // * array length equal to hash length
    // * array correctly shrink additional slot's when they non needed

    let entity1 = entity_id::invalid_new(1);
    let entity237 = entity_id::invalid_new(237);
    let entity438 = entity_id::invalid_new(438);
    let entity500 = entity_id::invalid_new(500);
    let entity700 = entity_id::invalid_new(700);

    let mut array: SparseArray<EntityId> = SparseArray::new();

    // we still can pack 4 more items
    array.set(entity1);
    assert_eq!(array.hash, 2);
    assert_eq!(array.data_slots, 1);
    assert_eq!(array.data_slots_end, 2);
    assert_eq!(array.get(entity1), &entity1);

    // we still can pack 3 more items
    array.set(entity237);
    assert_eq!(array.hash, 2);
    assert_eq!(array.data_slots, 2);
    assert_eq!(array.data_slots_end, 3);
    assert_eq!(array.get(entity237), &entity237);

    // we still can pack 2 more items
    array.set(entity438);
    assert_eq!(array.hash, 2);
    assert_eq!(array.data_slots, 3);
    assert_eq!(array.data_slots_end, 4);
    assert_eq!(array.get(entity438), &entity438);

    // we still can pack 1 more items
    array.set(entity500);
    assert_eq!(array.hash, 2);
    assert_eq!(array.data_slots, 4);
    assert_eq!(array.data_slots_end, 5);
    assert_eq!(array.get(entity500), &entity500);

    array.set(entity700);
    assert_eq!(array.hash, 11);
    assert_eq!(array.data_slots, 5);
    assert_eq!(array.data_slots_end, 10);
    assert_eq!(array.get(entity700), &entity700);

    // after re-scaling we still have access to valid values
    assert_eq!(array.get(entity1), &entity1);
    assert_eq!(array.get(entity237), &entity237);
    assert_eq!(array.get(entity438), &entity438);
    assert_eq!(array.get(entity500), &entity500);

    assert_eq!(array.hash_disabled, false);
}

#[test]
#[cfg_attr(miri, ignore)]
fn alot_of_items() {
    let mut array = SparseArray::<EntityId>::new();
    for i in 1u16..u16::MAX {
        let entity = entity_id::invalid_new(i);
        array.set(entity);
        assert_eq!(array.data_slots, i);
    }

    for i in 1u16..u16::MAX {
        let entity = entity_id::invalid_new(i);
        assert_eq!(array.get(entity).get_id(), entity.id());
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn no_hash() {
    let mut array = SparseArray::<EntityId>::new();
    array.hash_disabled = true;
    assert_eq!(array.hash_disabled, true);

    for i in 1u16..u16::MAX {
        let entity = entity_id::invalid_new(i);
        array.set(entity);
        assert_eq!(array.first_(entity.get_id().get()).get_id(), entity.id());
        assert_eq!(array.data_slots, i);
    }

    assert_eq!(array.hash_disabled, true);
    assert_eq!(array.hash, u16::MAX);
    assert_eq!(array.data_slots, u16::MAX - 1);
    assert_eq!(array.data_slots_end, u16::MAX);

    for i in 1u16..u16::MAX {
        let entity = entity_id::invalid_new(i);
        assert_eq!(array.first_(entity.id().get()).get_id(), entity.id());
        array.set(entity);
        assert_eq!(array.first_(entity.id().get()).get_id(), entity.id());
    }

    assert_eq!(array.hash, u16::MAX);
    assert_eq!(array.data_slots, u16::MAX - 1);
    assert_eq!(array.data_slots_end, u16::MAX);
}

#[test]
fn disabled_hashing() {
    // hash function disable hashing and use normal id's
    // when hash greater than MAX_HASH, this cause for increased
    // memory usage, but adding and deleting is always O(1)
    //
    // this test check that:
    // * when hash greater than > 4096 and no-free slot for value
    //   array starts insert values without hashing
    // * user may insert multiple values without re-hashing
    // * table correctly switch from hash to sparse variant
    // * inserting ignore u16::MAX value

    let entity32000 = entity_id::invalid_new(32000);
    let entity2371 = entity_id::invalid_new(2371);
    let entity4380 = entity_id::invalid_new(4380);
    let entity5000 = entity_id::invalid_new(5000);
    let entity4500 = entity_id::invalid_new(4500);

    let mut array: SparseArray<EntityId> = SparseArray::new();

    array.set(entity32000);
    array.set(entity2371);
    array.set(entity4380);
    array.set(entity5000);
    array.set(entity4500);

    assert_eq!(array.get(entity32000), &entity32000);
    assert_eq!(array.get(entity2371), &entity2371);
    assert_eq!(array.get(entity4380), &entity4380);
    assert_eq!(array.get(entity5000), &entity5000);
    assert_eq!(array.get(entity4500), &entity4500);

    assert_eq!(array.hash, 7);
    assert_eq!(array.data_slots, 5);
    assert_eq!(array.data_slots_end, 8);
    assert_eq!(array.hash_disabled, false);

    let mut total_items = 0;
    for i in (2..16400).step_by(2) {
        total_items += 1;
        let entity = entity_id::invalid_new(i);
        array.set(entity);
    }
    assert_eq!(total_items, 8199);
    assert_eq!(array.hash, 32001);
    assert_eq!(array.data_slots_end, 32001);
    assert_eq!(array.data_slots, total_items + 2);
    assert_eq!(array.hash_disabled, true);

    assert_eq!(array.get(entity32000), &entity32000);
    assert_eq!(array.get(entity2371), &entity2371);
    assert_eq!(array.get(entity4380), &entity4380);
    assert_eq!(array.get(entity5000), &entity5000);
    assert_eq!(array.get(entity4500), &entity4500);

    // slot with id=u16::MAX will be threated as empty or won't be inserted at all
    // assert_eq!(array.set(unsafe { NonZeroU16::new_unchecked(u16::MAX) }, entity4500), false);
    let entity7395 = entity_id::invalid_new(7395);
    array.set(entity7395);
    assert_eq!(array.get(entity7395), &entity7395);
    assert_eq!(array.hash, 32001);
    assert_eq!(array.data_slots_end, 32001);
    assert_eq!(array.data_slots, total_items + 3);
}

#[test]
fn multi_collision_hashing() {
    // to make array dense as possible, when hash greater than 50,
    // we allow more than one collision, this is possible by packing
    // conflicting id's into array holes
    //
    // this feature cause get_first method, sometimes return invalid values
    // for cases where you need to be sure that value not present in array
    // please use get_(), get_mut_()
    //
    // this tests check that:
    // * maximum pack distance for values is array less than MAX_PACK_DISTANCE
    // * get_(), get_mut_() methods correctly return values

    let entity45 = entity_id::invalid_new(45);
    let entity98 = entity_id::invalid_new(98);
    let entity151 = entity_id::invalid_new(151);
    let mut array: SparseArray<EntityId> = SparseArray::new();

    array.set(entity98);
    array.set(entity45);

    // inserting more entities that collide between themself
    for i in 1..=45 {
        let entity = entity_id::invalid_new(i);
        array.set(entity);
    }
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 46);
    assert_eq!(array.data_slots_end, 47);

    // array can fit elements with id=45 and id=98 even when they conflicting
    // but this cause misses with get method
    assert_eq!(array.first_(45).get_id().get(), 45);
    assert_eq!(array.first_(98).get_id().get(), 45);

    assert_eq!(unsafe { array.data.get_unchecked(46) }.get_id().get(), 98);

    // inserting new element with index() == 98 only updates existed value
    let custom_e98 = entity_id::new(98, 32);
    array.set(custom_e98);
    assert_eq!(unsafe { array.data.get_unchecked(46) }.arch().get(), 32);
    assert_eq!(array.data_slots, 46);

    // array re-use existed hash because slot for id=47 is free
    let entity48 = entity_id::invalid_new(48);
    array.set(entity48);
    assert_eq!(array.hash, 53);

    // array will be reuse hash 53, because we still can pack 151 with distance
    // less than MAX_PACK_DISTANCE
    array.set(entity151);
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 48);
    assert_eq!(array.data_slots_end, 49);

    // sequnce insert
    let mut array: SparseArray<EntityId> = SparseArray::new();
    for i in 0..100 {
        let entity = entity_id::invalid_new((i + 1) * (i + 1));
        array.set(entity);
    }
    assert_eq!(array.hash_disabled, false);

    assert_eq!(array.data_slots, 100);
    assert_eq!(array.hash, 211);

    // DEBUG:
    //
    // for idx in 1..array.data_last_index {
    //     let id = unsafe { array.data.get_unchecked(idx as usize) }.get_id().get();
    //     if id == u16::MAX {
    //         continue;
    //     }
    //     println!("{idx} in array == {id}");
    // }
}

#[test]
fn delete_multi_collision_hashing() {
    let mut array: SparseArray<EntityId> = SparseArray::new();

    // 45, 98, 151 collide when hash=53
    let entity45 = entity_id::invalid_new(45);
    let entity98 = entity_id::invalid_new(98);
    let entity151 = entity_id::new(151, 32);

    // delete non-exists / invalid entities
    assert_eq!(array.delete(entity151.id()), None);
    assert_eq!(array.data_slots, 0);
    assert_eq!(array.delete(unsafe { NonZeroU16::new_unchecked(u16::MAX) }), None);
    assert_eq!(array.data_slots, 0);

    for i in 1..=46 {
        let entity = entity_id::invalid_new(i);
        array.set(entity);
    }
    array.set(entity45);
    array.set(entity98);

    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 47);
    assert_eq!(array.data_slots_end, 48);

    assert_eq!(array.delete(entity151.id()), None);
    assert_eq!(array.data_slots, 47);
    assert_eq!(array.data_slots_end, 48);
    assert_eq!(array.delete(unsafe { NonZeroU16::new_unchecked(u16::MAX) }), None);
    assert_eq!(array.data_slots, 47);
    assert_eq!(array.data_slots_end, 48);

    array.set(entity151);
    assert_eq!(array.data_slots, 48);
    assert_eq!(array.data_slots_end, 49);

    assert_eq!(array.first_(45).get_id().get(), 45);
    assert_eq!(array.first_(98).get_id().get(), 45);
    assert_eq!(array.first_(151).get_id().get(), 45);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 45);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), 151);

    assert_eq!(array.data_slots_max, 151);
    assert_eq!(array.data_slots_min, 1);

    assert_eq!(array.delete(entity45.id()), Some(entity45));
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 47);
    // INFO: last index remains same because new value at index 45 is 98 (closest to 45) and not 151
    assert_eq!(array.data_slots_end, 49);

    assert_eq!(array.first_(98).get_id().get(), 98);
    assert_eq!(array.first_(151).get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), 151);

    array.set(entity45);
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 48);
    assert_eq!(array.data_slots_end, 49);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 45);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), 151);

    assert_eq!(array.delete(entity45.id()), Some(entity45));
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 47);
    assert_eq!(array.data_slots_end, 49);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), 151);
    assert_eq!(array.delete(entity45.id()), None);

    // changing value with index() = 151 should be safe
    let entity151 = entity_id::new(151, 64);
    array.set(entity151);
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 47);
    assert_eq!(array.data_slots_end, 49);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), 151);

    assert_eq!(array.delete(entity151.id()), Some(entity151));
    assert_eq!(array.delete(entity151.id()), None);
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 46);
    assert_eq!(array.data_slots_end, 47);
    assert_eq!(unsafe { array.data.get_unchecked(46) }.get_id().get(), 46);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), 98);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), u16::MAX);

    assert_eq!(array.delete(entity_id::invalid_new(1).id()), Some(entity_id::invalid_new(1)));
    assert_eq!(array.delete(entity98.id()), Some(entity98));
    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 44);
    assert_eq!(array.data_slots_end, 47);
    assert_eq!(unsafe { array.data.get_unchecked(45) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(46) }.get_id().get(), 46);
    assert_eq!(unsafe { array.data.get_unchecked(47) }.get_id().get(), u16::MAX);
    assert_eq!(unsafe { array.data.get_unchecked(48) }.get_id().get(), u16::MAX);

    assert_eq!(array.data_slots_max, 46);
    assert_eq!(array.data_slots_min, 2);
}

#[test]
fn delete_no_collision() {
    let mut array = SparseArray::<EntityId>::new();
    array.hash_disabled = true;

    for i in 1u16..100 {
        let entity = entity_id::invalid_new(i * i);
        array.set(entity);
        assert_eq!(array.first_(i * i).get_id(), entity.id());
        assert_eq!(array.data_slots, i);
        assert_eq!(array.data_slots_max, i * i);
    }

    let expected_hash = 2u16.pow((((99 * 99) as f32).log2().ceil() + 1.0) as u32);
    assert_eq!(array.hash, expected_hash);
    assert_eq!(array.data_slots, 99);
    assert_eq!(array.data_slots_end, (99 * 99) + 1);

    assert_eq!(array.delete(unsafe { NonZeroU16::new_unchecked(99 * 99 + 1) }), None);
    assert_eq!(
        array.delete(unsafe { NonZeroU16::new_unchecked(99 * 99) }),
        Some(entity_id::invalid_new(99 * 99))
    );
    assert_eq!(array.data_slots, 98);
    assert_eq!(array.data_slots_end, 221);
    assert_ne!(array.get_(99 * 99).get_id().get(), 99 * 99);
    array.set(entity_id::invalid_new(99 * 99));

    for i in 1u16..100 {
        let entity = entity_id::invalid_new(i * i);
        assert_eq!(array.data_slots_min, i * i);
        if array.delete(entity.id()).is_some() {
            assert_eq!(array.data_slots, 99 - i);
            for z in (i + 1)..100 {
                assert_eq!(array.get_(z * z).id().get(), z * z, "can't find {} in {}", z, i);
            }
        } else {
            panic!("can't delete {}", i);
        }
    }
    assert_eq!(array.data_slots, 0);
    assert_eq!(array.data_slots_end, 1);
    assert_eq!(array.hash, 29);
    assert_eq!(array.first_(1).get_id().get(), INVALID_SLOT_ID);
    assert_eq!(array.first_(99 * 99).get_id().get(), INVALID_SLOT_ID);
}

#[test]
fn set_bit_on_hash_disabled() {
    let mut array = SparseArray::new();
    array.hash_disabled = true;
    assert_eq!(array.set(entity_id::invalid_new(100)), SetOpResult::UpsertHashDisabled);
    assert_eq!(array.data_slots_max, 100);
    assert_eq!(array.data_slots_min, 100);
}

#[test]
fn set_bit_on_insert_rehash() {
    let mut array = SparseArray::new();
    assert_eq!(array.set(entity_id::invalid_new(100)), SetOpResult::InsertRehash);
    assert_eq!(array.data_slots_max, 100);
    assert_eq!(array.data_slots_min, 100);
}

#[test]
fn set_bit_on_insert_min() {
    let mut array = SparseArray::new();
    assert_eq!(array.set(entity_id::invalid_new(100)), SetOpResult::InsertRehash);
    assert_eq!(array.set(entity_id::invalid_new(2)), SetOpResult::Insert);
    assert_eq!(array.data_slots_min, 2);
    assert_eq!(array.data_slots_max, 100);
}

#[test]
fn set_bit_on_insert_packed_max() {
    let mut array = SparseArray::new();
    assert_eq!(array.set(entity_id::invalid_new(100)), SetOpResult::InsertRehash);
    assert_eq!(array.set(entity_id::invalid_new(101)), SetOpResult::Insert);
    assert_eq!(array.data_slots_min, 100);
    assert_eq!(array.data_slots_max, 101);
}

#[test]
fn delete_bit() {
    let mut array = SparseArray::new();
    let _ = (array.set(entity_id::invalid_new(100)), array.set(entity_id::invalid_new(101)));
    assert_eq!((array.data_slots_min, array.data_slots_max), (100, 101));

    array.delete(entity_id::invalid_new(100).get_id());
    assert_eq!(array.data_slots_min, 101);
    assert_eq!(array.data_slots_max, 101);

    array.delete(entity_id::invalid_new(101).get_id());
    assert_eq!(array.data_slots_min, 0);
    assert_eq!(array.data_slots_max, 0);
}

#[test]
fn shrink_when_hash_disabled() {
    let mut array = SparseArray::new();
    array.hash_disabled = true;

    array.set(entity_id::invalid_new(3000));
    array.set(entity_id::invalid_new(4000));

    let actual_capacity = array.data_capacity.capacity();

    assert_eq!(array.hash, array.data_capacity.capacity());
    assert_eq!(array.hash_disabled, true);
    assert_eq!(array.data_slots, 2);
    assert_eq!(array.data_slots_end, 4001);

    // delete value and trigger shrinking
    assert_eq!(
        array.delete(entity_id::invalid_new(4000).get_id()),
        Some(entity_id::invalid_new(4000))
    );

    assert_eq!(array.data_capacity.capacity(), actual_capacity);
    assert_eq!(array.hash_disabled, false);
    assert_eq!(array.data_slots, 1);
    assert_eq!(array.data_slots_end, 5);
    assert_eq!(array.hash, 7);
}

#[test]
fn shrink_when_hash() {
    let mut array = SparseArray::new();
    for i in 1..=200 {
        array.set(entity_id::invalid_new(i));
    }
    assert_eq!(array.data_capacity, PackedCapacity(8));
    assert_eq!(array.hash_disabled, false);
    assert_eq!(array.data_slots, 200);
    assert_eq!(array.data_slots_end, 201);
    assert_eq!(array.hash, 211);

    for i in (3..=200).rev() {
        array.delete(entity_id::invalid_new(i).get_id());
    }
    assert_eq!(array.data_capacity, PackedCapacity(8));
    assert_eq!(array.hash_disabled, false);
    assert_eq!(array.data_slots, 2);
    assert_eq!(array.data_slots_end, 3);
    assert_eq!(array.hash, 29);
}

#[test]
#[cfg_attr(miri, ignore)]
fn sparse_integration() {
    // * this test all possible insertion types
    // * include transition from hash -> hash disabled
    //  and all deletion types and backward transition from hash disabled -> hash variant

    let mut array = SparseArray::new();
    for i in (2..=16400).step_by(2) {
        assert_ne!(array.set(entity_id::invalid_new(i)), SetOpResult::InvalidId);
        assert_eq!(array.data_slots_min, 2);
        assert_eq!(array.data_slots_max, i);
        assert_eq!(packed_capacity(i / BITS_SIZE + 1), array.bits_capacity,);
    }

    for i in (2..=16400).step_by(2) {
        assert_ne!(array.delete(entity_id::invalid_new(i).get_id()), None);
        assert_eq!(array.data_slots_min, if i == 16400 { 0 } else { i + 2 });
        assert_eq!(array.data_slots_max, if i == 16400 { 0 } else { 16400 });
    }

    assert_eq!(packed_capacity(16400 / BITS_SIZE + 1), array.bits_capacity);
    assert_eq!(array.data_slots, 0);
}

#[test]
fn get_with_packing() {
    // 45, 98, 151 collide when hash=53
    let mut array: SparseArray<EntityId> = SparseArray::new();
    let entity45 = entity_id::invalid_new(45);
    let entity98 = entity_id::invalid_new(98);
    let entity151 = entity_id::new(151, 32);
    for i in 1..=46 {
        let entity = entity_id::invalid_new(i);
        array.set(entity);
    }
    array.set(entity45);
    array.set(entity98);
    array.set(entity151);

    assert_eq!(array.hash, 53);
    assert_eq!(array.data_slots, 48);
    assert_eq!(array.data_slots_end, 49);

    assert_eq!(array.get_(entity45.id().get()), &entity45);
    assert_eq!(array.get_(entity98.id().get()), &entity98);
    assert_eq!(array.get_(entity151.id().get()), &entity151);

    assert_eq!(array.get_mut_(entity45.id().get()), &entity45);
    assert_eq!(array.get_mut_(entity98.id().get()), &entity98);
    assert_eq!(array.get_mut_(entity151.id().get()), &entity151);
}

#[test]
fn component_size() {
    assert_eq!(2u16.saturating_pow(16), u16::MAX);
    assert_eq!(std::mem::size_of::<SparseArray<EntityId>>(), 32);
}

#[test]
fn capacity_size() {
    assert_eq!(packed_capacity(0).capacity(), 1);
    assert_eq!(packed_capacity(1).capacity(), 1);
    assert_eq!(packed_capacity(2).capacity(), 2);
    assert_eq!(packed_capacity(3).capacity(), 4);
    assert_eq!(packed_capacity(113).capacity(), 128);
    assert_eq!(packed_capacity(128).capacity(), 128);
    assert_eq!(packed_capacity(129).capacity(), 256);

    let mut nums: Vec<usize> = Vec::new();
    RawVecU16::push(&mut nums, 0);
    assert_eq!(nums.capacity(), 2);

    RawVecU16::push(&mut nums, 1);
    assert_eq!(nums.capacity(), 2);

    RawVecU16::push(&mut nums, 3);
    assert_eq!(nums.capacity(), 4);

    RawVecU16::push(&mut nums, 4);
    assert_eq!(nums.capacity(), 4);

    RawVecU16::push(&mut nums, 5);
    assert_eq!(nums.capacity(), 8);

    for i in 0..120 {
        RawVecU16::push(&mut nums, i);
    }
    assert_eq!(nums.capacity(), 128);

    RawVecU16::push(&mut nums, 126);
    RawVecU16::push(&mut nums, 127);
    RawVecU16::push(&mut nums, 128);
    assert_eq!(nums.capacity(), 128);

    RawVecU16::push(&mut nums, 129);
    assert_eq!(nums.capacity(), 256);
}
