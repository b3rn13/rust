use crate::schema::*;

use rustc::hir::def_id::{DefId, DefIndex, DefIndexAddressSpace};
use rustc_serialize::opaque::Encoder;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use log::debug;

/// While we are generating the metadata, we also track the position
/// of each DefIndex. It is not required that all definitions appear
/// in the metadata, nor that they are serialized in order, and
/// therefore we first allocate the vector here and fill it with
/// `0`. Whenever an index is visited, we fill in the
/// appropriate spot by calling `record_position`. We should never
/// visit the same index twice.
pub struct Table<'tcx> {
    positions: [Vec<u8>; 2],
    _marker: PhantomData<&'tcx ()>,
}

impl Table<'tcx> {
    pub fn new((max_index_lo, max_index_hi): (usize, usize)) -> Self {
        Table {
            positions: [vec![0; max_index_lo * 4],
                        vec![0; max_index_hi * 4]],
            _marker: PhantomData,
        }
    }

    pub fn record(&mut self, def_id: DefId, entry: Lazy<Entry<'tcx>>) {
        assert!(def_id.is_local());
        self.record_index(def_id.index, entry);
    }

    pub fn record_index(&mut self, item: DefIndex, entry: Lazy<Entry<'tcx>>) {
        let position = entry.position.get() as u32;
        assert_eq!(position as usize, entry.position.get());
        let space_index = item.address_space().index();
        let array_index = item.as_array_index();

        let destination = &mut self.positions[space_index][array_index * 4..];
        assert!(read_le_u32(destination) == 0,
                "recorded position for item {:?} twice, first at {:?} and now at {:?}",
                item,
                read_le_u32(destination),
                position);

        write_le_u32(destination, position);
    }

    pub fn encode(&self, buf: &mut Encoder) -> Lazy<[Self]> {
        let pos = buf.position();

        // First we write the length of the lower range ...
        buf.emit_raw_bytes(&(self.positions[0].len() as u32 / 4).to_le_bytes());
        // ... then the values in the lower range ...
        buf.emit_raw_bytes(&self.positions[0]);
        // ... then the values in the higher range.
        buf.emit_raw_bytes(&self.positions[1]);
        Lazy::from_position_and_meta(
            NonZeroUsize::new(pos as usize).unwrap(),
            (self.positions[0].len() + self.positions[1].len()) / 4 + 1,
        )
    }
}

impl Lazy<[Table<'tcx>]> {
    /// Given the metadata, extract out the offset of a particular
    /// DefIndex (if any).
    #[inline(never)]
    pub fn lookup(&self, bytes: &[u8], def_index: DefIndex) -> Option<Lazy<Entry<'tcx>>> {
        debug!("Table::lookup: index={:?} len={:?}",
               def_index,
               self.meta);

        let i = def_index.as_array_index() + match def_index.address_space() {
            DefIndexAddressSpace::Low => 0,
            DefIndexAddressSpace::High => {
                // This is a DefIndex in the higher range, so find out where
                // that starts:
                read_le_u32(&bytes[self.position.get()..]) as usize
            }
        };

        let position = read_le_u32(&bytes[self.position.get() + (1 + i) * 4..]);
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
