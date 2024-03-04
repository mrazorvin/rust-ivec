use crate::sync_sparse_array::sync_array;

mod ivec;
mod sync_sparse_array;
mod sync_vec;

use hashbrown::raw::RawTable;

fn main() {
    // ivec immutable sync friendly ordered vec
    let ivec: ivec::IVec<usize> = ivec::IVec::new();

    let mut table: RawTable<u64> = RawTable::new();
    for i in 0..100 {
        let value = (i * i) | (i * i) << 32;
        table.insert(value, value, |v| *v);
    }

    let mut slots: Vec<usize> = Vec::new();
    for i in 0..100 {
        let value = (i * i) | (i * i) << 32;
        let bucket = &table.find(value, |v| *v == value).unwrap();
        let index = unsafe { table.bucket_index(bucket) };
        slots.push(index);
        // println!("Slots for  {} -> {:?}", value, index);
    }
    slots.sort_by_key(|v| *v);
    println!("All occipied slots: {:?}", slots);
    println!("Total buckets: {:?}", table.buckets());
    println!("Total capacity: {:?}", table.capacity());

    ivec.get_or_insert(4, &|| 4);
    ivec.get_or_insert(4, &|| 4);
    ivec.get_or_insert(2, &|| 2);
    ivec.get_or_insert(1, &|| 1);
    ivec.get_or_insert(3, &|| 3);

    println!("{:?}", unsafe { ivec.as_slice() });

    // sparse array
    let sparse = sync_array();
    let _prev_value = sparse.set_in_place(1234, String::from("My value for id 1234"));
    let _read_only_bucket = sparse.bucket(1234);

    println!("{:?}", unsafe { _read_only_bucket.get_unchecked(1234) });

    println!("bucket bits: {:b}", sparse.bits(1234));
    println!("min        : {:?}", sparse.min_relaxed());
    println!("min        : {:?}", sparse.max_relaxed());

    let _deleted_value = sparse.delete_in_place(1234);

    let mut x = [1; 24];
    let (x, y) = x.split_at_mut(5);
    // let z = &mut x[0];
    // let y = &mut y[0];

    println!("{:?}", _deleted_value);
}
