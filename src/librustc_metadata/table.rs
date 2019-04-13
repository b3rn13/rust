use crate::schema::*;

use rustc::hir::def_id::{DefId, DefIndex, DefIndexAddressSpace};
use rustc_serialize::{Encodable, opaque::Encoder};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use log::debug;

/// Random-access position table, allowing encoding in an arbitrary order
/// (e.g. while visiting the definitions of a crate), and on-demand decoding
/// of specific indices (e.g. queries for per-definition data).
/// Similar to `Vec<Lazy<T>>`, but with zero-copy decoding.
// FIXME(eddyb) newtype `[u8]` here, such that `Box<Table<T>>` would be used
// when building it, and `Lazy<Table<T>>` or `&Table<T>` when reading it.
// Sadly, that doesn't work for `DefPerTable`, which is `(Table<T>, Table<T>)`,
// and so would need two lengths in its metadata, which is not supported yet.
pub struct Table<T: LazyMeta<Meta = ()>> {
    bytes: Vec<u8>,
    _marker: PhantomData<T>,
}

impl<T: LazyMeta<Meta = ()>> Table<T> {
    pub fn new(len: usize) -> Self {
        Table {
            bytes: vec![0; len * 4],
            _marker: PhantomData,
        }
    }

    pub fn record(&mut self, i: usize, entry: Lazy<T>) {
        let position = entry.position.get() as u32;
        assert_eq!(position as usize, entry.position.get());

        let bytes = &mut self.bytes[i * 4..];
        assert!(read_le_u32(bytes) == 0,
                "recorded position for index {:?} twice, first at {:?} and now at {:?}",
                i,
                read_le_u32(bytes),
                position);

        write_le_u32(bytes, position);
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

impl<T: LazyMeta<Meta = ()>> LazyMeta for Table<T> {
    type Meta = usize;

    fn min_size(len: usize) -> usize {
        len
    }
}

impl<T: Encodable> Lazy<Table<T>> {
    /// Given the metadata, extract out the offset of a particular index (if any).
    #[inline(never)]
    pub fn lookup(&self, bytes: &[u8], i: usize) -> Option<Lazy<T>> {
        debug!("Table::lookup: index={:?} len={:?}", i, self.meta);

        let bytes = &bytes[self.position.get()..][..self.meta];
        let position = read_le_u32(&bytes[i * 4..]);
        debug!("Table::lookup: position={:?}", position);

        NonZeroUsize::new(position as usize).map(Lazy::from_position)
    }
}

fn read_le_u32(b: &[u8]) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&b[..4]);
    u32::from_le_bytes(bytes)
}

fn write_le_u32(b: &mut [u8], x: u32) {
    b[..4].copy_from_slice(&x.to_le_bytes());
}

/// Per-definition table, similar to `Table` but keyed on `DefIndex`.
/// Needed because of the two `DefIndexAddressSpace`s a `DefIndex` can be in.
pub struct PerDefTable<T: LazyMeta<Meta = ()>> {
    lo: Table<T>,
    hi: Table<T>,
}

impl<T: LazyMeta<Meta = ()>> PerDefTable<T> {
    pub fn new((max_index_lo, max_index_hi): (usize, usize)) -> Self {
        PerDefTable {
            lo: Table::new(max_index_lo),
            hi: Table::new(max_index_hi),
        }
    }

    pub fn record(&mut self, def_id: DefId, entry: Lazy<T>) {
        assert!(def_id.is_local());
        let space_index = def_id.index.address_space().index();
        let array_index = def_id.index.as_array_index();
        [&mut self.lo, &mut self.hi][space_index].record(array_index, entry);
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

impl<T: LazyMeta<Meta = ()>> LazyMeta for PerDefTable<T> {
    type Meta = [<Table<T> as LazyMeta>::Meta; 2];

    fn min_size([lo, hi]: Self::Meta) -> usize {
        Table::<T>::min_size(lo) + Table::<T>::min_size(hi)
    }
}

impl<T: Encodable> Lazy<PerDefTable<T>> {
    fn table_for_space(&self, space: DefIndexAddressSpace) -> Lazy<Table<T>> {
        let space_index = space.index();
        let offset = space_index.checked_sub(1).map_or(0, |i| self.meta[i]);
        Lazy::from_position_and_meta(
            NonZeroUsize::new(self.position.get() + offset).unwrap(),
            self.meta[space_index]
        )
    }

    /// Given the metadata, extract out the offset of a particular DefIndex (if any).
    #[inline(never)]
    pub fn lookup(&self, bytes: &[u8], def_index: DefIndex) -> Option<Lazy<T>> {
        self.table_for_space(def_index.address_space())
            .lookup(bytes, def_index.as_array_index())
    }
}

