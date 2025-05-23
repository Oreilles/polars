use polars_compute::rolling::QuantileMethod;

use super::*;

pub trait QuantileAggSeries {
    /// Get the median of the [`ChunkedArray`] as a new [`Series`] of length 1.
    fn median_reduce(&self) -> Scalar;
    /// Get the quantile of the [`ChunkedArray`] as a new [`Series`] of length 1.
    fn quantile_reduce(&self, _quantile: f64, _method: QuantileMethod) -> PolarsResult<Scalar>;
}

/// helper
fn quantile_idx(
    quantile: f64,
    length: usize,
    null_count: usize,
    method: QuantileMethod,
) -> (usize, f64, usize) {
    let nonnull_count = (length - null_count) as f64;
    let float_idx = (nonnull_count - 1.0) * quantile + null_count as f64;
    let mut base_idx = match method {
        QuantileMethod::Nearest => {
            let idx = float_idx.round() as usize;
            return (idx, 0.0, idx);
        },
        QuantileMethod::Lower | QuantileMethod::Midpoint | QuantileMethod::Linear => {
            float_idx as usize
        },
        QuantileMethod::Higher => float_idx.ceil() as usize,
        QuantileMethod::Equiprobable => {
            let idx = ((nonnull_count * quantile).ceil() - 1.0).max(0.0) as usize + null_count;
            return (idx, 0.0, idx);
        },
    };

    base_idx = base_idx.clamp(0, length - 1);
    let top_idx = f64::ceil(float_idx) as usize;

    (base_idx, float_idx, top_idx)
}

/// helper
fn linear_interpol<T: Float>(lower: T, upper: T, idx: usize, float_idx: f64) -> T {
    if lower == upper {
        lower
    } else {
        let proportion: T = T::from(float_idx).unwrap() - T::from(idx).unwrap();
        proportion * (upper - lower) + lower
    }
}
fn midpoint_interpol<T: Float>(lower: T, upper: T) -> T {
    if lower == upper {
        lower
    } else {
        (lower + upper) / (T::one() + T::one())
    }
}

// Uses quickselect instead of sorting all data
fn quantile_slice<T: ToPrimitive + TotalOrd + Copy>(
    vals: &mut [T],
    quantile: f64,
    method: QuantileMethod,
) -> PolarsResult<Option<f64>> {
    polars_ensure!((0.0..=1.0).contains(&quantile),
        ComputeError: "quantile should be between 0.0 and 1.0",
    );
    if vals.is_empty() {
        return Ok(None);
    }
    if vals.len() == 1 {
        return Ok(vals[0].to_f64());
    }
    let (idx, float_idx, top_idx) = quantile_idx(quantile, vals.len(), 0, method);

    let (_lhs, lower, rhs) = vals.select_nth_unstable_by(idx, TotalOrd::tot_cmp);
    if idx == top_idx {
        Ok(lower.to_f64())
    } else {
        match method {
            QuantileMethod::Midpoint => {
                let upper = rhs.iter().copied().min_by(TotalOrd::tot_cmp).unwrap();
                Ok(Some(midpoint_interpol(
                    lower.to_f64().unwrap(),
                    upper.to_f64().unwrap(),
                )))
            },
            QuantileMethod::Linear => {
                let upper = rhs.iter().copied().min_by(TotalOrd::tot_cmp).unwrap();
                Ok(linear_interpol(
                    lower.to_f64().unwrap(),
                    upper.to_f64().unwrap(),
                    idx,
                    float_idx,
                )
                .to_f64())
            },
            _ => Ok(lower.to_f64()),
        }
    }
}

fn generic_quantile<T>(
    ca: ChunkedArray<T>,
    quantile: f64,
    method: QuantileMethod,
) -> PolarsResult<Option<f64>>
where
    T: PolarsNumericType,
{
    polars_ensure!(
        (0.0..=1.0).contains(&quantile),
        ComputeError: "`quantile` should be between 0.0 and 1.0",
    );

    let null_count = ca.null_count();
    let length = ca.len();

    if null_count == length {
        return Ok(None);
    }

    let (idx, float_idx, top_idx) = quantile_idx(quantile, length, null_count, method);
    let sorted = ca.sort(false);
    let lower = sorted.get(idx).map(|v| v.to_f64().unwrap());

    let opt = match method {
        QuantileMethod::Midpoint => {
            if top_idx == idx {
                lower
            } else {
                let upper = sorted.get(idx + 1).map(|v| v.to_f64().unwrap());
                midpoint_interpol(lower.unwrap(), upper.unwrap()).to_f64()
            }
        },
        QuantileMethod::Linear => {
            if top_idx == idx {
                lower
            } else {
                let upper = sorted.get(idx + 1).map(|v| v.to_f64().unwrap());

                linear_interpol(lower.unwrap(), upper.unwrap(), idx, float_idx).to_f64()
            }
        },
        _ => lower,
    };
    Ok(opt)
}

impl<T> ChunkQuantile<f64> for ChunkedArray<T>
where
    T: PolarsIntegerType,
    T::Native: TotalOrd,
{
    fn quantile(&self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f64>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        if let (Ok(slice), false) = (self.cont_slice(), self.is_sorted_ascending_flag()) {
            let mut owned = slice.to_vec();
            quantile_slice(&mut owned, quantile, method)
        } else {
            generic_quantile(self.clone(), quantile, method)
        }
    }

    fn median(&self) -> Option<f64> {
        self.quantile(0.5, QuantileMethod::Linear).unwrap() // unwrap fine since quantile in range
    }
}

// Version of quantile/median that don't need a memcpy
impl<T> ChunkedArray<T>
where
    T: PolarsIntegerType,
    T::Native: TotalOrd,
{
    pub(crate) fn quantile_faster(
        mut self,
        quantile: f64,
        method: QuantileMethod,
    ) -> PolarsResult<Option<f64>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        let is_sorted = self.is_sorted_ascending_flag();
        if let (Some(slice), false) = (self.cont_slice_mut(), is_sorted) {
            quantile_slice(slice, quantile, method)
        } else {
            self.quantile(quantile, method)
        }
    }

    pub(crate) fn median_faster(self) -> Option<f64> {
        self.quantile_faster(0.5, QuantileMethod::Linear).unwrap()
    }
}

impl ChunkQuantile<f32> for Float32Chunked {
    fn quantile(&self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f32>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        let out = if let (Ok(slice), false) = (self.cont_slice(), self.is_sorted_ascending_flag()) {
            let mut owned = slice.to_vec();
            quantile_slice(&mut owned, quantile, method)
        } else {
            generic_quantile(self.clone(), quantile, method)
        };
        out.map(|v| v.map(|v| v as f32))
    }

    fn median(&self) -> Option<f32> {
        self.quantile(0.5, QuantileMethod::Linear).unwrap() // unwrap fine since quantile in range
    }
}

impl ChunkQuantile<f64> for Float64Chunked {
    fn quantile(&self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f64>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        if let (Ok(slice), false) = (self.cont_slice(), self.is_sorted_ascending_flag()) {
            let mut owned = slice.to_vec();
            quantile_slice(&mut owned, quantile, method)
        } else {
            generic_quantile(self.clone(), quantile, method)
        }
    }

    fn median(&self) -> Option<f64> {
        self.quantile(0.5, QuantileMethod::Linear).unwrap() // unwrap fine since quantile in range
    }
}

impl Float64Chunked {
    pub(crate) fn quantile_faster(
        mut self,
        quantile: f64,
        method: QuantileMethod,
    ) -> PolarsResult<Option<f64>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        let is_sorted = self.is_sorted_ascending_flag();
        if let (Some(slice), false) = (self.cont_slice_mut(), is_sorted) {
            quantile_slice(slice, quantile, method)
        } else {
            self.quantile(quantile, method)
        }
    }

    pub(crate) fn median_faster(self) -> Option<f64> {
        self.quantile_faster(0.5, QuantileMethod::Linear).unwrap()
    }
}

impl Float32Chunked {
    pub(crate) fn quantile_faster(
        mut self,
        quantile: f64,
        method: QuantileMethod,
    ) -> PolarsResult<Option<f32>> {
        // in case of sorted data, the sort is free, so don't take quickselect route
        let is_sorted = self.is_sorted_ascending_flag();
        if let (Some(slice), false) = (self.cont_slice_mut(), is_sorted) {
            quantile_slice(slice, quantile, method).map(|v| v.map(|v| v as f32))
        } else {
            self.quantile(quantile, method)
        }
    }

    pub(crate) fn median_faster(self) -> Option<f32> {
        self.quantile_faster(0.5, QuantileMethod::Linear).unwrap()
    }
}

impl ChunkQuantile<String> for StringChunked {}
impl ChunkQuantile<Series> for ListChunked {}
#[cfg(feature = "dtype-array")]
impl ChunkQuantile<Series> for ArrayChunked {}
#[cfg(feature = "object")]
impl<T: PolarsObject> ChunkQuantile<Series> for ObjectChunked<T> {}
impl ChunkQuantile<bool> for BooleanChunked {}
