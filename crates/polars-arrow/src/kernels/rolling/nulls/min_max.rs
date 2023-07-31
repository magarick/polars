use nulls;
use nulls::{rolling_apply_agg_window, RollingAggWindowNulls};

use super::*;

#[inline]
fn new_is_min<T: NativeType + IsFloat + PartialOrd>(old: &T, new: &T) -> bool {
    compare_fn_nan_min(old, new).is_ge()
}

#[inline]
fn new_is_max<T: NativeType + IsFloat + PartialOrd>(old: &T, new: &T) -> bool {
    compare_fn_nan_max(old, new).is_le()
}

// Identify runs of sorted values, either increasing or decreasing. It lets us avoid unnecessary
// comparisons when the extremum keeps dropping off the front of the window
macro_rules! fn_n_sorted_past {
    ($name:ident, $cmp_f:ident, $stop_ord:ident) => {
        #[inline]
        unsafe fn $name<T: NativeType + IsFloat + PartialOrd>(
            slice: &[T],
            validity: &Bitmap,
            m_idx: usize,
        ) -> usize {
            let (mut last_val, mut n) = (slice.get_unchecked(m_idx), 0);
            for i in (m_idx + 1)..slice.len() {
                n += 1;
                if validity.get_bit_unchecked(i) {
                    let val = slice.get_unchecked(i);
                    if $cmp_f(last_val, val) == Ordering::$stop_ord {
                        break;
                    }
                    last_val = val;
                }
            }
            n + m_idx
        }
    };
}
fn_n_sorted_past!(n_sorted_past_min, compare_fn_nan_min, Greater);
fn_n_sorted_past!(n_sorted_past_max, compare_fn_nan_max, Less);

// Min and max really are the same thing up to a difference in comparison direction, as represented
// here by helpers we pass in. Making both with a macro helps keep behavior synchronized
macro_rules! minmax_window {
    ($m_window:tt, $new_is_m:ident, $n_sorted_past:ident, $rolling_fn:ident) => {
        pub struct $m_window<'a, T: NativeType + PartialOrd + IsFloat> {
            slice: &'a [T],
            validity: &'a Bitmap,
            m: Option<T>,
            m_idx: usize,
            sorted_to: usize,
            last_start: usize,
            last_end: usize,
            null_count: usize,
        }

        impl<'a, T: NativeType + IsFloat + PartialOrd> $m_window<'a, T> {
            #[inline]
            unsafe fn init_get_m_m_idx_and_null_count(
                slice: &'a [T],
                validity: &'a Bitmap,
                start: usize,
                end: usize,
                sorted_to: usize,
            ) -> (Option<(usize, &'a T)>, usize) {
                let ilen = end - start;
                let leading_nc = validity
                    .iter()
                    .skip(start)
                    .take(ilen)
                    .position(|x| x)
                    .unwrap_or(ilen);
                if leading_nc == ilen {
                    return (None, ilen);
                }
                let vstart = start + leading_nc; // First _V_alid entry start
                let (mut m, mut m_idx) = (slice.get_unchecked(vstart), vstart);
                if sorted_to >= end {
                    let remaining_nc = validity.null_count_range(vstart, end - vstart);
                    (Some((m_idx, m)), leading_nc + remaining_nc)
                } else {
                    let tstart = (vstart + 1).max(sorted_to); // _T_ail elements to check
                    let mut remaining_nc = validity.null_count_range(vstart, tstart - vstart);
                    for i in tstart..end {
                        if !validity.get_bit_unchecked(i) {
                            remaining_nc += 1;
                            continue;
                        }
                        let newval = slice.get_unchecked(i);
                        if $new_is_m(m, newval) {
                            (m, m_idx) = (newval, i);
                        }
                    }
                    (Some((m_idx, m)), remaining_nc + leading_nc)
                }
            }

            #[inline]
            unsafe fn get_m_m_idx_and_null_count(
                &self,
                start: usize,
                end: usize,
            ) -> (Option<(usize, &'a T)>, usize) {
                // Make it a little more convenient to call from within an update
                $m_window::init_get_m_m_idx_and_null_count(
                    self.slice,
                    self.validity,
                    start,
                    end,
                    self.sorted_to,
                )
            }

            #[inline]
            unsafe fn update_m_and_m_idx(&mut self, m_and_idx: (usize, &T)) {
                self.m = Some(*m_and_idx.1);
                self.m_idx = m_and_idx.0;
                if self.sorted_to <= self.m_idx {
                    // Track how far past the current extremum values are sorted. Direction depends on min/max
                    // Tracking sorted ranges lets us only do comparisons when we have to.
                    self.sorted_to = $n_sorted_past(&self.slice, self.validity, self.m_idx);
                }
            }
        }

        impl<'a, T: NativeType + IsFloat + PartialOrd> RollingAggWindowNulls<'a, T>
            for $m_window<'a, T>
        {
            unsafe fn new(
                slice: &'a [T],
                validity: &'a Bitmap,
                start: usize,
                end: usize,
                _params: DynArgs,
            ) -> Self {
                let (m_idx, null_count) =
                    $m_window::init_get_m_m_idx_and_null_count(slice, validity, start, end, 0);
                let (m, idx) = (m_idx.map(|x| x.1), m_idx.map_or(0, |x| x.0));
                let sorted_to = m_idx.map_or(1, |x| $n_sorted_past(&slice, validity, x.0));
                Self {
                    slice,
                    validity,
                    m: m.copied(),
                    m_idx: idx,
                    sorted_to: sorted_to,
                    last_start: start,
                    last_end: end,
                    null_count,
                }
            }

            unsafe fn update(&mut self, start: usize, end: usize) -> Option<T> {
                //For details see: https://github.com/pola-rs/polars/pull/9277#issuecomment-1581401692
                let leaving_nc = self
                    .validity
                    .null_count_range(self.last_start, start - self.last_start);
                self.last_start = start; // Don't care where the last one started now
                let old_last_end = self.last_end; // But we need this
                self.last_end = end;
                let entering_start = std::cmp::max(old_last_end, start);
                let (entering, entering_nc) = if end - entering_start == 1 {
                    // Faster in the special, but common, case of a fixed window rolling by one
                    if self.validity.get_bit_unchecked(entering_start) {
                        (
                            Some((entering_start, self.slice.get_unchecked(entering_start))),
                            0,
                        )
                    } else {
                        (None, 1)
                    }
                } else {
                    self.get_m_m_idx_and_null_count(entering_start, end)
                };
                let empty_overlap = old_last_end <= start;
                self.null_count = (self.null_count + entering_nc) - leaving_nc;

                if entering.is_some_and(|em| {
                    self.m.is_none() || empty_overlap || $new_is_m(&self.m.unwrap(), em.1)
                }) {
                    // The entering extremum "beats" the previous extremum so we can ignore the overlap
                    self.update_m_and_m_idx(entering.unwrap());
                    return self.m;
                } else if self.m_idx >= start || empty_overlap {
                    // The previous extremum didn't drop off. Keep it
                    return self.m;
                }
                // Otherwise get the min of the overlapping window and the entering extremum
                // If the last value was None, the value in the overlapping window will be too
                let (previous, _) = match self.m {
                    None => (None, 0),
                    Some(_) => self.get_m_m_idx_and_null_count(start, old_last_end),
                };
                match (previous, entering) {
                    (Some(pm), Some(em)) => {
                        if $new_is_m(pm.1, em.1) {
                            self.update_m_and_m_idx(em);
                        } else {
                            self.update_m_and_m_idx(pm);
                        }
                    }
                    (Some(pm), None) => self.update_m_and_m_idx(pm),
                    (None, Some(em)) => self.update_m_and_m_idx(em),
                    // Both the entering and previous windows are empty or all null
                    (None, None) => self.m = None,
                }

                self.m
            }
            fn is_valid(&self, min_periods: usize) -> bool {
                ((self.last_end - self.last_start) - self.null_count) >= min_periods
            }
        }

        pub fn $rolling_fn<T>(
            arr: &PrimitiveArray<T>,
            window_size: usize,
            min_periods: usize,
            center: bool,
            weights: Option<&[f64]>,
            _params: DynArgs,
        ) -> ArrayRef
        where
            T: NativeType + Zero + Copy + PartialOrd + Bounded + IsFloat,
        {
            if weights.is_some() {
                panic!("weights not yet supported on array with null values")
            }
            let offset_fn = match center {
                true => det_offsets_center,
                false => det_offsets,
            };
            rolling_apply_agg_window::<$m_window<_>, _, _>(
                arr.values().as_slice(),
                arr.validity().as_ref().unwrap(),
                window_size,
                min_periods,
                offset_fn,
                None,
            )
        }
    };
}

minmax_window!(MinWindow, new_is_min, n_sorted_past_min, rolling_min);
minmax_window!(MaxWindow, new_is_max, n_sorted_past_max, rolling_max);
