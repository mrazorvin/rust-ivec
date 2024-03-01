use crate::sync_sparse_array::sync_array;

mod ivec;
mod sync_sparse_array;

use ivec::*;
use sync_sparse_array::*;

fn main() {
    // ivec immutable sync friendly ordered vec
    let ivec: ivec::IVec<usize> = ivec::IVec::new();

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

    println!("{:?}", _deleted_value);
}
