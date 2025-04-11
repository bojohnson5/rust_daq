use ndarray::Array2;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct CEvent {
    pub timestamp: u64,
    pub timestamp_us: f64,
    pub trigger_id: u32,
    pub event_size: usize,
    // waveform is an array of pointers (one per channel)
    pub waveform: *mut *mut u16,
    // Arrays (one element per channel) filled in by the C function
    pub n_samples: *mut usize,
    pub n_allocated_samples: *mut usize,
    pub n_channels: usize,
}

/// A safe wrapper that owns the allocated memory for a CEvent.
///
/// The inner `c_event` field can be passed to the C function, while the owned
/// buffers are automatically dropped when the wrapper goes out of scope.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EventWrapper {
    pub c_event: CEvent,

    // The waveform data is stored as a 2D contiguous array.
    pub waveform_data: Array2<u16>,
    // Owned slice of waveform pointers. We need to keep this alive so that
    // `c_event.waveform` (a raw pointer into it) remains valid.
    waveform_ptrs: Box<[*mut u16]>,
    // Owned memory for the per-channel arrays.
    n_samples: Box<[usize]>,
    n_allocated_samples: Box<[usize]>,
}

unsafe impl Send for EventWrapper {}

impl EventWrapper {
    /// Create a new EventWrapper.
    ///
    /// # Arguments
    ///
    /// * `n_channels` - Number of waveforms/channels.
    /// * `waveform_len` - Number of samples per waveform.
    pub fn new(n_channels: usize, waveform_len: usize) -> Self {
        // Create a 2D array for waveform data with dimensions (n_channels, waveform_len).
        let mut waveform_data = Array2::<u16>::zeros((n_channels, waveform_len));

        // Build a vector of pointers—one per row.
        let mut waveform_ptrs_vec = Vec::with_capacity(n_channels);
        for i in 0..n_channels {
            // Get a mutable pointer to the row i.
            let row_ptr = waveform_data.as_mut_ptr().wrapping_add(i * waveform_len);
            waveform_ptrs_vec.push(row_ptr);
        }
        let mut waveform_ptrs = waveform_ptrs_vec.into_boxed_slice();

        // Allocate the arrays for n_samples and n_allocated_samples.
        // Here we assume that each channel is allocated with `waveform_len` samples.
        let mut n_samples = vec![0usize; n_channels].into_boxed_slice();
        let mut n_allocated_samples = vec![waveform_len; n_channels].into_boxed_slice();

        // Get mutable raw pointers to pass to the C API.
        let waveform_ptr = waveform_ptrs.as_mut_ptr();
        let n_samples_ptr = n_samples.as_mut_ptr();
        let n_allocated_samples_ptr = n_allocated_samples.as_mut_ptr();

        // Build the C-compatible event.
        let c_event = CEvent {
            timestamp: 0,
            timestamp_us: 0.0,
            trigger_id: 0,
            event_size: 0,
            waveform: waveform_ptr,
            n_samples: n_samples_ptr,
            n_allocated_samples: n_allocated_samples_ptr,
            n_channels,
        };

        Self {
            c_event,
            waveform_data,
            waveform_ptrs,
            n_samples,
            n_allocated_samples,
        }
    }
}
