//! Asm — a thin label/register helper over hlbc `Opcode` emission.
//!
//! Hand-emitting HashLink opcodes for the injector meant computing every relative
//! jump offset by hand (`*offset = target - site - 1`) and hand-allocating each
//! register by number — the two error classes that repeatedly broke the spawn/move
//! bytecode (silent mis-jumps / clobbered regs, no stack trace). `Asm` removes both:
//!
//!  - **Branches target named [`Label`]s.** Emit `asm.jfalse(r, lbl)`, `asm.jalways(lbl)`,
//!    … against a label you `place()` wherever you like (forward or backward). `finish()`
//!    resolves every offset in one pass and panics on an unplaced/duplicate label —
//!    turning a class of runtime engine crashes into a build-time error.
//!  - **Registers are allocated, not numbered.** `asm.reg(ty)` hands out a fresh register;
//!    `finish()` returns the list of types to feed `add_regs`.
//!
//! The emitted bytecode is byte-identical to the equivalent hand-written ops, so an
//! `Asm` block is a drop-in for a self-contained sub-sequence spliced into an existing
//! op stream (all its jumps are internal/relative). Cross-block jumps are intentionally
//! NOT supported — keep a block self-contained, or fall through off its end.

use hlbc::opcodes::Opcode;
use hlbc::types::Reg;

/// A jump target within one [`Asm`] block. Opaque; only valid for the `Asm` that made it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Label(usize);

/// Accumulates opcodes with label-relative branches and allocated registers.
pub struct Asm {
    ops: Vec<Opcode>,
    reg_base: u32,
    reg_types: Vec<usize>,
    label_pos: Vec<Option<usize>>, // label id -> op index (None until placed)
    fixups: Vec<(usize, usize)>,   // (jump op index, label id)
}

impl Asm {
    /// `reg_base` MUST equal `f.regs.len()` at the point the returned `reg_types` will be
    /// appended via `add_regs` (i.e. don't allocate other regs in between), so the
    /// register indices `reg()` hands out line up with where they actually land.
    pub fn new(reg_base: u32) -> Self {
        Asm { ops: Vec::new(), reg_base, reg_types: Vec::new(), label_pos: Vec::new(), fixups: Vec::new() }
    }

    /// Allocate a fresh register of engine type index `ty`. Returns its `Reg`.
    pub fn reg(&mut self, ty: usize) -> Reg {
        let r = Reg(self.reg_base + self.reg_types.len() as u32);
        self.reg_types.push(ty);
        r
    }

    /// Create a label (unplaced). Place it later with [`Asm::place`].
    pub fn label(&mut self) -> Label {
        let id = self.label_pos.len();
        self.label_pos.push(None);
        Label(id)
    }

    /// Point `l` at the NEXT op to be emitted (the current end of the stream).
    pub fn place(&mut self, l: Label) {
        debug_assert!(self.label_pos[l.0].is_none(), "asm: label placed twice");
        self.label_pos[l.0] = Some(self.ops.len());
    }

    /// Emit a raw (non-branch) opcode.
    pub fn op(&mut self, op: Opcode) { self.ops.push(op); }

    fn branch(&mut self, l: Label, op: Opcode) {
        self.fixups.push((self.ops.len(), l.0));
        self.ops.push(op);
    }
    pub fn jalways(&mut self, l: Label) { self.branch(l, Opcode::JAlways { offset: 0 }); }
    pub fn jnull(&mut self, reg: Reg, l: Label) { self.branch(l, Opcode::JNull { reg, offset: 0 }); }
    pub fn jnotnull(&mut self, reg: Reg, l: Label) { self.branch(l, Opcode::JNotNull { reg, offset: 0 }); }
    pub fn jslt(&mut self, a: Reg, b: Reg, l: Label) { self.branch(l, Opcode::JSLt { a, b, offset: 0 }); }
    pub fn jeq(&mut self, a: Reg, b: Reg, l: Label) { self.branch(l, Opcode::JEq { a, b, offset: 0 }); }
    pub fn jtrue(&mut self, cond: Reg, l: Label) { self.branch(l, Opcode::JTrue { cond, offset: 0 }); }

    /// Resolve all label jumps to relative offsets and return `(ops, reg_types)`.
    /// `reg_types` go straight to `add_regs(f, &reg_types)`. Panics on an unplaced label.
    pub fn finish(mut self) -> (Vec<Opcode>, Vec<usize>) {
        for (idx, lbl) in &self.fixups {
            let target = self.label_pos[*lbl]
                .unwrap_or_else(|| panic!("asm: label {lbl} jumped to but never placed"));
            let off = target as i32 - *idx as i32 - 1;
            match &mut self.ops[*idx] {
                Opcode::JAlways { offset }
                | Opcode::JTrue { offset, .. }
                | Opcode::JFalse { offset, .. }
                | Opcode::JNull { offset, .. }
                | Opcode::JNotNull { offset, .. }
                | Opcode::JSGte { offset, .. }
                | Opcode::JSLt { offset, .. }
                | Opcode::JEq { offset, .. }
                | Opcode::JNotEq { offset, .. } => *offset = off,
                _ => unreachable!("asm: fixup recorded on a non-jump opcode"),
            }
        }
        (self.ops, self.reg_types)
    }
}
