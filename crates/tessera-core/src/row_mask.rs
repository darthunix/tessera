use anyhow::{Context, Result, ensure};

use crate::bitmap::{validate_row, validate_words, word_count};

#[derive(Clone, Copy, Debug)]
enum Storage<'a> {
    Words(&'a [u64]),
    Bytes { bytes: &'a [u8], bit_offset: usize },
}

// Keep partial and shifted byte-window decoding out of the full-word hot path.
// A shifted word inside the buffer is two fixed-size loads; only the last
// bytes of the buffer are copied byte by byte.
#[inline(never)]
fn byte_word(bytes: &[u8], bit_offset: usize, word_index: usize, nrows: usize) -> u64 {
    let window = &bytes[word_index * 8..];
    let bits = if let Some(nine) = window.get(..9) {
        let low = u64::from_le_bytes(nine[..8].try_into().unwrap());
        if bit_offset == 0 {
            low
        } else {
            (low >> bit_offset) | (u64::from(nine[8]) << (64 - bit_offset))
        }
    } else {
        let mut data = [0; 8];
        data[..window.len()].copy_from_slice(window);
        u64::from_le_bytes(data) >> bit_offset
    };
    let valid = (nrows - word_index * 64).min(64);
    bits & (u64::MAX >> (64 - valid))
}

/// A read-only, borrowed bitmap of physical rows.
///
/// Set bits can represent active rows or non-NULL values. Row selection and
/// non-NULL tracking use separate bitmaps, even though they share this view type.
///
/// Bits are numbered least significant first in both words and bytes.
/// [`Self::try_new`] accepts strict `TessRowMask`-style words, not its C struct
/// layout. [`Self::try_from_bytes`] borrows an arbitrary bit window, including
/// sliced Arrow non-NULL masks. Neither constructor copies or owns storage.
///
/// A view cannot outlive its storage:
///
/// ```compile_fail
/// use tessera_core::RowMaskView;
/// let rows;
/// {
///     let words = [1];
///     rows = RowMaskView::try_new(1, &words).unwrap();
/// }
/// assert_eq!(rows.selected_count(), 1);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct RowMaskView<'a> {
    nrows: usize,
    storage: Storage<'a>,
}

impl<'a> RowMaskView<'a> {
    /// Borrow words, rejecting an incorrect word count or nonzero padding bits.
    pub fn try_new(nrows: usize, words: &'a [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self {
            nrows,
            storage: Storage::Words(words),
        })
    }

    /// Borrow `nrows` bits starting at `bit_offset`, without alignment needs.
    ///
    /// Bits outside the window are ignored, including nonzero padding.
    /// Overflow or a window beyond the buffer returns an error. An empty
    /// window may start at any valid position, including the buffer's end.
    pub fn try_from_bytes(nrows: usize, bytes: &'a [u8], bit_offset: usize) -> Result<Self> {
        let end = bit_offset
            .checked_add(nrows)
            .context("bitmap bit range overflows")?;
        ensure!(
            end.div_ceil(8) <= bytes.len(),
            "bitmap bit range exceeds its buffer"
        );
        Ok(Self {
            nrows,
            storage: Storage::Bytes {
                bytes: &bytes[bit_offset / 8..end.div_ceil(8)],
                bit_offset: bit_offset % 8,
            },
        })
    }

    /// Return a logical 64-row word, or `None` beyond the row count.
    ///
    /// The last word's padding is always zero, independent of backing format.
    #[inline]
    pub fn word(&self, word_index: usize) -> Option<u64> {
        if word_index >= word_count(self.nrows) {
            return None;
        }
        Some(match self.storage {
            Storage::Words(words) => words[word_index],
            Storage::Bytes { bytes, bit_offset } => {
                if bit_offset == 0 && self.nrows - word_index * 64 >= 64 {
                    let start = word_index * 8;
                    // A fixed-size copy permits a direct load without requiring
                    // u64 alignment or reading beyond the validated window.
                    u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap())
                } else {
                    byte_word(bytes, bit_offset, word_index, self.nrows)
                }
            }
        })
    }

    /// Return the number of physical rows, including unselected rows.
    pub fn nrows(&self) -> usize {
        self.nrows
    }

    /// Return the number of selected rows, not the physical row count.
    pub fn selected_count(&self) -> usize {
        (0..word_count(self.nrows))
            .map(|index| self.word(index).unwrap().count_ones() as usize)
            .sum()
    }

    /// Check a physical row, returning an error if `row >= self.nrows()`.
    #[inline]
    pub fn contains(&self, row: usize) -> Result<bool> {
        validate_row(row, self.nrows)?;
        Ok(match self.storage {
            Storage::Words(words) => words[row / 64] & (1_u64 << (row % 64)) != 0,
            Storage::Bytes { bytes, bit_offset } => {
                let bit = row + bit_offset;
                bytes[bit / 8] & (1_u8 << (bit % 8)) != 0
            }
        })
    }

    /// Visit selected physical indices in increasing order without allocating.
    ///
    /// Only set bits are visited within each word. Once exhausted, the
    /// iterator keeps returning `None`.
    pub fn selected_indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..word_count(self.nrows)).flat_map(|word| {
            let mut remaining = self.word(word).unwrap();
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let row = word * 64 + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                Some(row)
            })
        })
    }
}

/// An exclusively borrowed row bitmap that can only remove rows.
///
/// Construction validates the invariants of [`RowMaskView::try_new`]. Successful
/// operations do not allocate; creating an error may allocate. No operation
/// frees or resizes the backing storage. Dropping the mask ends the borrow;
/// changes remain in the caller's words.
///
/// Two mutable views cannot access the same storage simultaneously:
///
/// ```compile_fail
/// use tessera_core::RowMask;
/// let mut words = [1];
/// let mut first = RowMask::try_new(1, &mut words).unwrap();
/// let second = RowMask::try_new(1, &mut words).unwrap();
/// first.clear(0);
/// assert_eq!(second.as_view().selected_count(), 0);
/// ```
#[derive(Debug)]
pub struct RowMask<'a> {
    nrows: usize,
    words: &'a mut [u64],
}

impl<'a> RowMask<'a> {
    /// Borrow words exclusively, rejecting bad dimensions or padding bits.
    ///
    /// Validation never modifies the supplied words, including on error.
    pub fn try_new(nrows: usize, words: &'a mut [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self { nrows, words })
    }

    /// Reborrow for reading; mutation is forbidden while that view is in use.
    ///
    /// ```compile_fail
    /// use tessera_core::RowMask;
    /// let mut words = [1];
    /// let mut rows = RowMask::try_new(1, &mut words).unwrap();
    /// let view = rows.as_view();
    /// rows.clear(0);
    /// assert_eq!(view.selected_count(), 1);
    /// ```
    pub fn as_view(&self) -> RowMaskView<'_> {
        RowMaskView {
            nrows: self.nrows,
            storage: Storage::Words(self.words),
        }
    }

    /// Remove a physical row; an absent or out-of-bounds row is ignored.
    ///
    /// Padding bits are already zero after construction, so clearing one
    /// leaves the words unchanged. Indices beyond the words are also ignored.
    #[inline]
    pub fn clear(&mut self, row: usize) {
        if let Some(word) = self.words.get_mut(row / 64) {
            *word &= !(1_u64 << (row % 64));
        }
    }

    /// Intersect one 64-row word, without restoring any cleared row.
    ///
    /// An absent word returns an error before any mutation, even when `bits`
    /// is zero. Other words stay unchanged. Padding bits in `bits` are harmless:
    /// intersection preserves the zero padding validated by the constructor.
    ///
    /// ```
    /// use tessera_core::RowMask;
    /// let mut words = [0b111, 1];
    /// let mut rows = RowMask::try_new(65, &mut words)?;
    /// rows.intersect_word(0, 0b101)?;
    /// rows.intersect_word(1, u64::MAX)?;
    /// assert_eq!(rows.as_view().selected_indices().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    #[inline]
    pub fn intersect_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .context("mask word is out of bounds")?;
        *word &= bits;
        Ok(())
    }

    /// Replace one 64-row word; this is the one operation that can restore
    /// rows, for masks a kernel produces rather than narrows.
    ///
    /// An absent word or set padding bits in `bits` return an error before
    /// any mutation. Other words stay unchanged.
    ///
    /// ```
    /// use tessera_core::RowMask;
    /// let mut words = [0, 0];
    /// let mut rows = RowMask::try_new(65, &mut words)?;
    /// rows.set_word(0, 0b101)?;
    /// rows.set_word(1, 1)?;
    /// assert!(rows.set_word(1, 0b10).is_err());
    /// assert_eq!(rows.as_view().selected_indices().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    #[inline]
    pub fn set_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .context("mask word is out of bounds")?;
        let valid = (self.nrows - index * 64).min(64);
        ensure!(
            valid == 64 || bits >> valid == 0,
            "mask word has set padding bits"
        );
        *word = bits;
        Ok(())
    }

    /// Remove rows absent from `other`, without restoring any cleared row.
    ///
    /// Different physical row counts return an error before any mutation.
    pub fn intersect(&mut self, other: RowMaskView<'_>) -> Result<()> {
        ensure!(
            self.nrows == other.nrows,
            "expected {} physical rows, got {}",
            self.nrows,
            other.nrows
        );
        for (index, word) in self.words.iter_mut().enumerate() {
            *word &= other.word(index).unwrap();
        }
        Ok(())
    }
}
