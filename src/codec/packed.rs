//! The one place that knows a dictionary index is stored in 16 bits.

/// A dictionary index stored in 16 bits, with the top value reserved for
/// "none". `Field` holds three of these, and while each spelled its own guard
/// one of them drifted into an unchecked cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packed16(u16);

impl Packed16 {
    pub(crate) const NONE: Packed16 = Packed16(u16::MAX);

    /// The largest index that packs. A component that *generates* indices — the
    /// group annotator — bounds itself by this rather than producing one that
    /// cannot be stored.
    pub(crate) const MAX_VALUE: usize = u16::MAX as usize - 1;

    /// `NONE` when the value cannot be represented: asserts in debug and
    /// degrades to "unknown" in release rather than aliasing a real index.
    pub(crate) fn pack(value: u32) -> Packed16 {
        debug_assert!(
            value as usize <= Packed16::MAX_VALUE,
            "index {value} does not pack"
        );
        match u16::try_from(value) {
            Ok(raw) if raw != u16::MAX => Packed16(raw),
            _ => Packed16::NONE,
        }
    }

    pub(crate) fn get(self) -> Option<u32> {
        (self.0 != u16::MAX).then_some(u32::from(self.0))
    }
}

// The dictionary layer states the same width as a count, and cannot import this
// module. Tied together here, where both are visible.
const _: () = assert!(crate::dictionary::MAX_ITEMS == Packed16::MAX_VALUE + 1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_index_round_trips() {
        assert_eq!(Packed16::pack(0).get(), Some(0));
        assert_eq!(Packed16::pack(1).get(), Some(1));
        assert_eq!(
            Packed16::pack(Packed16::MAX_VALUE as u32).get(),
            Some(Packed16::MAX_VALUE as u32)
        );
    }

    #[test]
    fn none_is_not_a_position() {
        assert_eq!(Packed16::NONE.get(), None);
        assert_eq!(Packed16::MAX_VALUE, u16::MAX as usize - 1);
    }

    /// Release behaviour for a value the parsers are supposed to have made
    /// impossible: degrade to "unknown" rather than alias index 0 or panic.
    /// `pack` asserts in debug, so this asserts the release path only.
    #[test]
    #[cfg(not(debug_assertions))]
    fn an_unrepresentable_index_degrades_to_none() {
        assert_eq!(Packed16::pack(u32::MAX).get(), None);
        assert_eq!(Packed16::pack(u16::MAX as u32).get(), None);
    }
}
