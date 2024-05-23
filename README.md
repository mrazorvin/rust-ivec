Data structures created for https://github.com/mrazorvin/rust-ecs-public game engine

## Commands

- Tests `cargo test --lib -Zpanic-abort-tests` or `cargo nextest run --lib`
- Miri  `cargo miri nextest run -j4 --color=always --lib`

## Features

* `RawVecU16` - This is a vector optimized for a maximum capacity of `u16::MAX` elements, designed for a more efficient memory layout. Its API offers methods to store a pointer to the data separately from the length and its capacity. The capacity only requires 1 byte of memory, and the length requires 2 bytes, resulting in a memory usage that is at least half that of a standard vector.
* `SparseArray` - A HashMap-like structure optimized for sparse access, designed to store around 2000 elements with an efficient memory layout before switching to a vector layout. The maximum capacity is `u16::MAX` items. Features include:
    - Uses only 32 bytes of memory, which is two times less than a default hash map and just 33% more than a vector.
    - Tracks the elements with the highest and lowest IDs.
    - Contains a `bitset` for fast filtering across multiple `SparseArrays`.
    - Can shrink and expand, unlike a default hash map.
    - Linear layout optimized for CPU efficiency.
* `SyncIVec` - A fully lock-free vector-like structure designed for a small number of elements (< 1000). This structure is inspired by functional programming, where each new version of the vector points to the previous ones, allowing developers to access previous versions until they are cleared. Notes:
    - Pushing new values is very slow because it requires copying all previous values into the new vector and sorting them. In case of conflicts, it also deallocates de-sync vector copy.
    - The vector can only store values that implement the `Clone` auto-trait.
    - Removing a previous version of the vector is an unsafe operation, requiring the user to ensure no other references to the map exist during the process.
* `SyncMap` - A concurrent hashmap optimized for fast read access, featuring two buckets: one for shared access and another for tracking changes. Users must manually sync these buckets periodically to achieve lock-free read performance boost. Notes:
    - The map consumes significant memory (~128KB), making it impractical to store many instances.
    - Syncing the map is an unsafe operation, requiring the user to ensure no other references to the map exist during the process.
    - Tracking changes is relatively slow, so this map is not suitable for scenarios requiring frequent updates.
* `SyncSlotMap` - A lock-free implementation of `SlotMap` that supports pushing, deleting, and iterating over inserted values. It uses a double-buffer `SyncVec` for managing  free slots. Notes:
    - Supports keys in range from `u8` to `u128`.
* `SyncVec` - A concurrent, free push-only vector with the first chunk allocated on the stack, implemented as a linked list of array chunks. Notes:
    - Memory locations are stable and can be safely referenced.
    - Supports only push and reset operations.

## Utils

* `disposable` - This trait indicates whether a struct requires disposal. For examaple: `ivec` needs to be disposed of at some point to clear all previous versions of the vector; otherwise, the application might run out of memory.
* `primes_u16` - A collection of all prime numbers up to `u16::MAX`


