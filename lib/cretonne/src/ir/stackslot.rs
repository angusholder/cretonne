//! Stack slots.
//!
//! The `StackSlotData` struct keeps track of a single stack slot in a function.
//!

use entity::{PrimaryMap, Keys};
use ir::{Type, StackSlot};
use std::fmt;
use std::ops::Index;
use std::str::FromStr;

/// The size of an object on the stack, or the size of a stack frame.
///
/// We don't use `usize` to represent object sizes on the target platform because Cretonne supports
/// cross-compilation, and `usize` is a type that depends on the host platform, not the target
/// platform.
pub type StackSize = u32;

/// A stack offset.
///
/// The location of a stack offset relative to a stack pointer or frame pointer.
pub type StackOffset = i32;

/// The kind of a stack slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackSlotKind {
    /// A spill slot. This is a stack slot created by the register allocator.
    SpillSlot,

    /// A local variable. This is a chunk of local stack memory for use by the `stack_load` and
    /// `stack_store` instructions.
    Local,

    /// An incoming function argument.
    ///
    /// If the current function has more arguments than fits in registers, the remaining arguments
    /// are passed on the stack by the caller. These incoming arguments are represented as SSA
    /// values assigned to incoming stack slots.
    IncomingArg,

    /// An outgoing function argument.
    ///
    /// When preparing to call a function whose arguments don't fit in registers, outgoing argument
    /// stack slots are used to represent individual arguments in the outgoing call frame. These
    /// stack slots are only valid while setting up a call.
    OutgoingArg,
}

impl FromStr for StackSlotKind {
    type Err = ();

    fn from_str(s: &str) -> Result<StackSlotKind, ()> {
        use self::StackSlotKind::*;
        match s {
            "local" => Ok(Local),
            "spill_slot" => Ok(SpillSlot),
            "incoming_arg" => Ok(IncomingArg),
            "outgoing_arg" => Ok(OutgoingArg),
            _ => Err(()),
        }
    }
}

impl fmt::Display for StackSlotKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::StackSlotKind::*;
        f.write_str(match *self {
            Local => "local",
            SpillSlot => "spill_slot",
            IncomingArg => "incoming_arg",
            OutgoingArg => "outgoing_arg",
        })
    }
}

/// Contents of a stack slot.
#[derive(Clone, Debug)]
pub struct StackSlotData {
    /// The kind of stack slot.
    pub kind: StackSlotKind,

    /// Size of stack slot in bytes.
    pub size: StackSize,

    /// Offset of stack slot relative to the stack pointer in the caller.
    ///
    /// On Intel ISAs, the base address is the stack pointer *before* the return address was
    /// pushed. On RISC ISAs, the base address is the value of the stack pointer on entry to the
    /// function.
    ///
    /// For `OutgoingArg` stack slots, the offset is relative to the current function's stack
    /// pointer immediately before the call.
    pub offset: StackOffset,
}

impl StackSlotData {
    /// Create a stack slot with the specified byte size.
    pub fn new(kind: StackSlotKind, size: StackSize) -> StackSlotData {
        StackSlotData {
            kind,
            size,
            offset: 0,
        }
    }

    /// Get the alignment in bytes of this stack slot given the stack pointer alignment.
    pub fn alignment(&self, max_align: StackSize) -> StackSize {
        debug_assert!(max_align.is_power_of_two());
        // We want to find the largest power of two that divides both `self.size` and `max_align`.
        // That is the same as isolating the rightmost bit in `x`.
        let x = self.size | max_align;
        // C.f. Hacker's delight.
        x & x.wrapping_neg()
    }
}

impl fmt::Display for StackSlotData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.kind, self.size)?;
        if self.offset != 0 {
            write!(f, ", offset {}", self.offset)?;
        }
        Ok(())
    }
}

/// Stack frame manager.
///
/// Keep track of all the stack slots used by a function.
#[derive(Clone, Debug)]
pub struct StackSlots {
    /// All allocated stack slots.
    slots: PrimaryMap<StackSlot, StackSlotData>,

    /// All the outgoing stack slots, ordered by offset.
    outgoing: Vec<StackSlot>,

    /// The total size of the stack frame.
    ///
    /// This is the distance from the stack pointer in the current function to the stack pointer in
    /// the calling function, so it includes a pushed return address as well as space for outgoing
    /// call arguments.
    ///
    /// This is computed by the `layout()` method.
    pub frame_size: Option<StackSize>,
}

/// Stack slot manager functions that behave mostly like an entity map.
impl StackSlots {
    /// Create an empty stack slot manager.
    pub fn new() -> StackSlots {
        StackSlots {
            slots: PrimaryMap::new(),
            outgoing: Vec::new(),
            frame_size: None,
        }
    }

    /// Clear out everything.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.outgoing.clear();
        self.frame_size = None;
    }

    /// Allocate a new stack slot.
    ///
    /// This function should be primarily used by the text format parser. There are more convenient
    /// functions for creating specific kinds of stack slots below.
    pub fn push(&mut self, data: StackSlotData) -> StackSlot {
        self.slots.push(data)
    }

    /// Check if `ss` is a valid stack slot reference.
    pub fn is_valid(&self, ss: StackSlot) -> bool {
        self.slots.is_valid(ss)
    }

    /// Set the offset of a stack slot.
    pub fn set_offset(&mut self, ss: StackSlot, offset: StackOffset) {
        self.slots[ss].offset = offset;
    }

    /// Get an iterator over all the stack slot keys.
    pub fn keys(&self) -> Keys<StackSlot> {
        self.slots.keys()
    }

    /// Get a reference to the next stack slot that would be created by `push()`.
    ///
    /// This should just be used by the parser.
    pub fn next_key(&self) -> StackSlot {
        self.slots.next_key()
    }
}

impl Index<StackSlot> for StackSlots {
    type Output = StackSlotData;

    fn index(&self, ss: StackSlot) -> &StackSlotData {
        &self.slots[ss]
    }
}

/// Higher-level stack frame manipulation functions.
impl StackSlots {
    /// Create a new spill slot for spilling values of type `ty`.
    pub fn make_spill_slot(&mut self, ty: Type) -> StackSlot {
        self.push(StackSlotData::new(StackSlotKind::SpillSlot, ty.bytes()))
    }

    /// Create a stack slot representing an incoming function argument.
    pub fn make_incoming_arg(&mut self, ty: Type, offset: StackOffset) -> StackSlot {
        let mut data = StackSlotData::new(StackSlotKind::IncomingArg, ty.bytes());
        assert!(offset <= StackOffset::max_value() - data.size as StackOffset);
        data.offset = offset;
        self.push(data)
    }

    /// Get a stack slot representing an outgoing argument.
    ///
    /// This may create a new stack slot, or reuse an existing outgoing stack slot with the
    /// requested offset and size.
    ///
    /// The requested offset is relative to this function's stack pointer immediately before making
    /// the call.
    pub fn get_outgoing_arg(&mut self, ty: Type, offset: StackOffset) -> StackSlot {
        let size = ty.bytes();

        // Look for an existing outgoing stack slot with the same offset and size.
        let inspos = match self.outgoing.binary_search_by_key(&(offset, size), |&ss| {
            (self[ss].offset, self[ss].size)
        }) {
            Ok(idx) => return self.outgoing[idx],
            Err(idx) => idx,
        };

        // No existing slot found. Make one and insert it into `outgoing`.
        let mut data = StackSlotData::new(StackSlotKind::OutgoingArg, size);
        assert!(offset <= StackOffset::max_value() - size as StackOffset);
        data.offset = offset;
        let ss = self.slots.push(data);
        self.outgoing.insert(inspos, ss);
        ss
    }
}

#[cfg(test)]
mod tests {
    use ir::Function;
    use ir::types;
    use super::*;

    #[test]
    fn stack_slot() {
        let mut func = Function::new();

        let ss0 = func.create_stack_slot(StackSlotData::new(StackSlotKind::IncomingArg, 4));
        let ss1 = func.create_stack_slot(StackSlotData::new(StackSlotKind::SpillSlot, 8));
        assert_eq!(ss0.to_string(), "ss0");
        assert_eq!(ss1.to_string(), "ss1");

        assert_eq!(func.stack_slots[ss0].size, 4);
        assert_eq!(func.stack_slots[ss1].size, 8);

        assert_eq!(func.stack_slots[ss0].to_string(), "incoming_arg 4");
        assert_eq!(func.stack_slots[ss1].to_string(), "spill_slot 8");
    }

    #[test]
    fn outgoing() {
        let mut sss = StackSlots::new();

        let ss0 = sss.get_outgoing_arg(types::I32, 8);
        let ss1 = sss.get_outgoing_arg(types::I32, 4);
        let ss2 = sss.get_outgoing_arg(types::I64, 8);

        assert_eq!(sss[ss0].offset, 8);
        assert_eq!(sss[ss0].size, 4);

        assert_eq!(sss[ss1].offset, 4);
        assert_eq!(sss[ss1].size, 4);

        assert_eq!(sss[ss2].offset, 8);
        assert_eq!(sss[ss2].size, 8);

        assert_eq!(sss.get_outgoing_arg(types::I32, 8), ss0);
        assert_eq!(sss.get_outgoing_arg(types::I32, 4), ss1);
        assert_eq!(sss.get_outgoing_arg(types::I64, 8), ss2);
    }

    #[test]
    fn alignment() {
        let slot = StackSlotData::new(StackSlotKind::SpillSlot, 8);

        assert_eq!(slot.alignment(4), 4);
        assert_eq!(slot.alignment(8), 8);
        assert_eq!(slot.alignment(16), 8);

        let slot2 = StackSlotData::new(StackSlotKind::Local, 24);

        assert_eq!(slot2.alignment(4), 4);
        assert_eq!(slot2.alignment(8), 8);
        assert_eq!(slot2.alignment(16), 8);
        assert_eq!(slot2.alignment(32), 8);
    }
}
