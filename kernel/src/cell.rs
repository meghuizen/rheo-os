//! The cell: address space + capability set + queues
//! (docs/ARCHITECTURE.md 3, object 1).
//!
//! At this stage a cell is a *protection context without a hardware
//! address space*: it has an identity and its own capability table, and
//! every kernel interaction is mediated by that table. The isolation
//! lemma is therefore checkable at the object-reachability level today
//! (disjoint tables = disjoint reachable objects); hardware address-space
//! enforcement arrives with BUILD-ORDER.md steps 3 and 5.

use crate::capability::CapTable;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CellId(pub u16);

pub struct Cell {
    pub id: CellId,
    pub caps: CapTable,
}

impl Cell {
    pub const fn new(id: u16) -> Cell {
        Cell {
            id: CellId(id),
            caps: CapTable::new(),
        }
    }
}
