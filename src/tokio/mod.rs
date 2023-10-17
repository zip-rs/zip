#![allow(missing_docs)]

pub mod buf_reader;
pub mod buf_writer;
pub mod channels;
pub mod combinators;
pub(crate) mod crc32;
pub(crate) mod extraction;
pub mod os;
pub mod read;
pub mod stream_impls;
pub mod write;

use std::pin::Pin;

pub trait WrappedPin<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>>;
}

pub(crate) mod utils {
    use std::{
        mem::{self, ManuallyDrop, MaybeUninit},
        ptr,
    };

    pub(crate) fn map_take_manual_drop<T, U, F: FnOnce(T) -> U>(
        slot: &mut ManuallyDrop<T>,
        f: F,
    ) -> ManuallyDrop<U> {
        let taken = unsafe { ManuallyDrop::take(slot) };
        ManuallyDrop::new(f(taken))
    }

    pub(crate) fn map_swap_uninit<T, F: FnOnce(T) -> T>(slot: &mut T, f: F) {
        let mut other = MaybeUninit::<T>::uninit();
        /* `other` IS UNINIT!!!!! */
        unsafe {
            ptr::swap(slot, other.as_mut_ptr());
        }
        /* `slot` IS NOW UNINIT!!!! */
        let mut wrapped = f(unsafe {
            /* `other` will be dropped at the end of f(). */
            other.assume_init()
        });
        /* `wrapped` has a valid value, returned by f(). */
        unsafe {
            ptr::swap(slot, &mut wrapped);
        }
        /* `wrapped` IS NOW UNINIT!!!! */
        mem::forget(wrapped);
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn test_map_swap_uninit_primitive() {
            let mut x = 5;
            map_swap_uninit(&mut x, |x| x + 3);
            assert_eq!(x, 8);
        }

        #[test]
        fn test_map_swap_uninit_heap_alloc() {
            let mut x: Vec<u8> = (0..12).map(|x| x * 3).collect();
            map_swap_uninit(&mut x, |mut x| {
                x.push(192);
                x.insert(0, 123);
                x
            });
            assert_eq!(
                x,
                vec![123, 0, 3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 192]
            );
        }
    }
}
