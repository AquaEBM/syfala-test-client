use core::{iter, mem, ptr::NonNull};

/// Data structure that allows interleaving samples without allocation to remain real-time safe
#[repr(transparent)]
pub struct Interleaver<T> {
    ptrs: [NonNull<T>],
}

// TODO: What's the safety argument here?
unsafe impl<T: Send + Sync> Send for Interleaver<T> {}

impl<T> Interleaver<T> {
    pub fn new(len: usize) -> Box<Self> {
        let boxed_slice = Box::from_iter(iter::repeat_with(NonNull::<T>::dangling).take(len));

        // SAFETY: We are a `#[repr(transparent)]` struct
        unsafe { mem::transmute(boxed_slice) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InterleaveError {
    IncorrectInputSliceLen,
    IncorrectOutputSliceLen,
    IncorrectInputSliceCount,
}

impl<T: Clone> Interleaver<T> {
    pub fn interleave<'a>(
        &'a mut self,
        n_frames: usize,
        slices: impl ExactSizeIterator<Item = &'a [T]>,
        output_slices: [&mut [mem::MaybeUninit<T>]; 2],
    ) -> Result<(), InterleaveError> {
        // write the pointers into our list

        let mut n_slices = 0;

        for (ptr, slice) in iter::zip(&mut self.ptrs, slices) {
            if n_frames != slice.len() {
                return Err(InterleaveError::IncorrectInputSliceLen);
            }

            n_slices += 1;

            // SAFETY: slice is a shared reference
            *ptr = unsafe { NonNull::new_unchecked(slice.as_ptr().cast_mut()) };
        }

        if n_slices != self.ptrs.len() {
            return Err(InterleaveError::IncorrectInputSliceCount);
        }

        let [start, end] = output_slices;
        if start.len() + end.len() != n_frames.checked_mul(n_slices).unwrap() {
            return Err(InterleaveError::IncorrectOutputSliceLen);
        }
        let mut output_slice_iter = iter::chain(start, end);

        for _ in 0..n_frames {
            for ptr in &mut self.ptrs {
                let val = unsafe { output_slice_iter.next().unwrap_unchecked() };

                // SAFETY: ptr points to a slice live for at least this whole function
                val.write(unsafe { ptr.as_ref() }.clone());

                // SAFETY: This is called at most n_frames times, making it within
                // the bounds of the slice it came from.
                *ptr = unsafe { ptr.add(1) };
            }
        }

        return Ok(());
    }
}
