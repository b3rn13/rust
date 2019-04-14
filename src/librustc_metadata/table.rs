use crate::decoder::Metadata;
use crate::schema::*;

use rustc::hir::def_id::{DefId, DefIndex, DefIndexAddressSpace};
use rustc_serialize::{Encodable, opaque::Encoder};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use log::debug;

/// Helper trait, for encoding to, and decoding from, a fixed number of bytes.
/// Used mainly for Lazy positions and lengths.
/// Unchecked invariant: `Self::default()` should encode as `[0; BYTE_LEN]`,
/// but this has no impact on safety.
pub trait FixedSizeEncoding: Default {
    const BYTE_LEN: usize;

    // FIXME(eddyb) convert to and from `[u8; Self::BYTE_LEN]` instead,
    // once that starts being allowed by the compiler (i.e. lazy normalization).
    fn from_bytes(b: &[u8]) -> Self;
    fn write_to_bytes(self, b: &mut [u8]);
}

impl FixedSizeEncoding for u32 {
    const BYTE_LEN: usize = 4;

    fn from_bytes(b: &[u8]) -> Self {
        let mut bytes = [0; Self::BYTE_LEN];
        bytes.copy_from_slice(&b[..Self::BYTE_LEN]);
        Self::from_le_bytes(bytes)
    }

    fn write_to_bytes(self, b: &mut [u8]) {
        b[..Self::BYTE_LEN].copy_from_slice(&self.to_le_bytes());
    }
}

// NOTE(eddyb) there could be an impl for `usize`, which would enable a more
// generic `Lazy<T>` impl, but in the general case we might not need / want to
// fit every `usize` in `u32`.
impl<T: Encodable> FixedSizeEncoding for Option<Lazy<T>> {
    const BYTE_LEN: usize = u32::BYTE_LEN;

    fn from_bytes(b: &[u8]) -> Self {
        Some(Lazy::from_position(NonZeroUsize::new(u32::from_bytes(b) as usize)?))
    }

    fn write_to_bytes(self, b: &mut [u8]) {
        let position = self.map_or(0, |lazy| lazy.position.get());
        let position_u32 = position as u32;
        assert_eq!(position_u32 as usize, position);

        position_u32.write_to_bytes(b)
    }
}

impl<T: Encodable> FixedSizeEncoding for Option<Lazy<[T]>> {
    const BYTE_LEN: usize = u32::BYTE_LEN * 2;

    fn from_bytes(b: &[u8]) -> Self {
        Some(Lazy::from_position_and_meta(
            <Option<Lazy<T>>>::from_bytes(b)?.position,
            u32::from_bytes(&b[u32::BYTE_LEN..]) as usize,
        ))
    }

    fn write_to_bytes(self, b: &mut [u8]) {
        self.map(|lazy| Lazy::<T>::from_position(lazy.position))
            .write_to_bytes(b);

        let len = self.map_or(0, |lazy| lazy.meta);
        let len_u32 = len as u32;
        assert_eq!(len_u32 as usize, len);

        len_u32.write_to_bytes(&mut b[u32::BYTE_LEN..]);
    }
}

/// Random-access table, similar to `Vec<Option<T>>`, but without requiring
/// encoding or decoding all the values eagerly and in-order.
// FIXME(eddyb) replace `Vec` with `[_]` here, such that `Box<Table<T>>` would be used
// when building it, and `Lazy<Table<T>>` or `&Table<T>` when reading it.
// Sadly, that doesn't work for `DefPerTable`, which is `(Table<T>, Table<T>)`,
// and so would need two lengths in its metadata, which is not supported yet.
pub struct Table<T> where Option<T>: FixedSizeEncoding {
    // FIXME(eddyb) store `[u8; <Option<T>>::BYTE_LEN]` instead of `u8` in `Vec`,
    // once that starts being allowed by the compiler (i.e. lazy normalization).
    bytes: Vec<u8>,
    _marker: PhantomData<T>,
}

impl<T> Table<T> where Option<T>: FixedSizeEncoding {
    pub fn new(len: usize) -> Self {
        Table {
            // FIXME(eddyb) only allocate and encode as many entries as needed.
            bytes: vec![0; len * <Option<T>>::BYTE_LEN],
            _marker: PhantomData,
        }
    }

    pub fn set(&mut self, i: usize, value: T) {
        Some(value).write_to_bytes(&mut self.bytes[i * <Option<T>>::BYTE_LEN..]);
    }

    pub fn encode(&self, buf: &mut Encoder) -> Lazy<Self> {
        let pos = buf.position();
        buf.emit_raw_bytes(&self.bytes);
        Lazy::from_position_and_meta(
            NonZeroUsize::new(pos as usize).unwrap(),
            self.bytes.len(),
        )
    }
}

impl<T> LazyMeta for Table<T> where Option<T>: FixedSizeEncoding {
    type Meta = usize;

    fn min_size(len: usize) -> usize {
        len
    }
}

impl<T> Lazy<Table<T>> where Option<T>: FixedSizeEncoding {
    /// Given the metadata, extract out the value at a particular index (if any).
    #[inline(never)]
    pub fn get<'a, 'tcx, M: Metadata<'a, 'tcx>>(
        &self,
        metadata: M,
        i: usize,
    ) -> Option<T> {
        debug!("Table::lookup: index={:?} len={:?}", i, self.meta);

        let bytes = &metadata.raw_bytes()[self.position.get()..][..self.meta];
        <Option<T>>::from_bytes(&bytes[i * <Option<T>>::BYTE_LEN..])
    }
}

/// Per-definition table, similar to `Table` but keyed on `DefIndex`.
/// Needed because of the two `DefIndexAddressSpace`s a `DefIndex` can be in.
pub struct PerDefTable<T> where Option<T>: FixedSizeEncoding {
    lo: Table<T>,
    hi: Table<T>,
}

impl<T> PerDefTable<T> where Option<T>: FixedSizeEncoding {
    pub fn new((max_index_lo, max_index_hi): (usize, usize)) -> Self {
        PerDefTable {
            lo: Table::new(max_index_lo),
            hi: Table::new(max_index_hi),
        }
    }

    pub fn set(&mut self, def_id: DefId, value: T) {
        assert!(def_id.is_local());
        let space_index = def_id.index.address_space().index();
        let array_index = def_id.index.as_array_index();
        [&mut self.lo, &mut self.hi][space_index].set(array_index, value);
    }

    pub fn encode(&self, buf: &mut Encoder) -> Lazy<Self> {
        let lo = self.lo.encode(buf);
        let hi = self.hi.encode(buf);
        assert_eq!(lo.position.get() + lo.meta, hi.position.get());

        Lazy::from_position_and_meta(
            lo.position,
            [lo.meta, hi.meta],
        )
    }
}

impl<T> LazyMeta for PerDefTable<T> where Option<T>: FixedSizeEncoding {
    type Meta = [<Table<T> as LazyMeta>::Meta; 2];

    fn min_size([lo, hi]: Self::Meta) -> usize {
        Table::<T>::min_size(lo) + Table::<T>::min_size(hi)
    }
}

impl<T> Lazy<PerDefTable<T>> where Option<T>: FixedSizeEncoding {
    fn table_for_space(&self, space: DefIndexAddressSpace) -> Lazy<Table<T>> {
        let space_index = space.index();
        let offset = space_index.checked_sub(1).map_or(0, |i| self.meta[i]);
        Lazy::from_position_and_meta(
            NonZeroUsize::new(self.position.get() + offset).unwrap(),
            self.meta[space_index]
        )
    }

    /// Given the metadata, extract out the value at a particular DefIndex (if any).
    #[inline(never)]
    pub fn get<'a, 'tcx, M: Metadata<'a, 'tcx>>(
        &self,
        metadata: M,
        def_index: DefIndex,
    ) -> Option<T> {
        self.table_for_space(def_index.address_space())
            .get(metadata, def_index.as_array_index())
    }
}
