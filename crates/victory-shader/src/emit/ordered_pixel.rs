//! Recognizes the console's primitive-ordered pixel shading protocol.
//!
//! The wait coordinates overlapping invocations of one pixel shader. D3D12
//! rasterizer-ordered views provide that ordering at the texture accesses.

use std::collections::HashSet;

use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::resource::{Site, inline};

use super::{Lower, V, Why, flags};

const PACKER: u16 = 0x1019; // HW_REG_POPS_PACKER, offset 0, width 3
const DONE: u16 = 7; // MSG_ORDERED_PS_DONE
pub(super) const EXITING_WAVE: u8 = 239;
pub(super) const ROV: u32 = 0x2000;

pub(super) struct OrderedPixel {
    pub overlap: Value,
    pub polls: [usize; 2],
    pub images: HashSet<Site>,
    branches: [usize; 2],
    resume: usize,
}

impl OrderedPixel {
    pub(super) fn recognize(program: &Program) -> Option<Self> {
        let mut markers = program.blocks.iter().enumerate().flat_map(|(b, block)| {
            block.insts.iter().enumerate().filter_map(move |(i, inst)| {
                (inst.name() == "s_setreg_b32"
                    && matches!(
                        inst.fields,
                        Fields::Sop {
                            imm: Some(PACKER),
                            ..
                        }
                    ))
                .then_some((b, i))
            })
        });
        let (header, marker) = markers.next()?;
        if markers.next().is_some() {
            return None;
        }
        let before = &program.blocks[header];
        let Exit::Branch {
            taken: resume,
            not_taken: wait,
        } = before.exit
        else {
            return None;
        };
        let spin = &program.blocks[wait];
        if before.insts.last()?.name() != "s_cbranch_vccnz"
            || spin.exit
                != (Exit::Branch {
                    taken: wait,
                    not_taken: resume,
                })
            || !spin.insts.iter().map(Inst::name).eq([
                "s_add_i32",
                "s_cmp_lt_u32",
                "s_cbranch_scc0",
            ])
        {
            return None;
        }
        let [first, repeated] = [
            before.insts[marker + 1..]
                .iter()
                .find(|i| reads_exit_wave(i))?,
            &spin.insts[0],
        ];
        if first.name() != "s_add_i32" || !reads_exit_wave(repeated) {
            return None;
        }
        let mut candidates = program.inputs.iter().filter_map(|&(reg, value)| {
            (matches!(reg, Reg::S(0..=105)) && overlap_fields(program, value)).then_some(value)
        });
        let overlap = candidates.next()?;
        if candidates.next().is_some()
            || !wait_uses_only(program, overlap, first, repeated, header, wait)
        {
            return None;
        }

        let mut work = vec![resume];
        let mut visited = HashSet::new();
        let mut images = HashSet::new();
        let mut ends = HashSet::new();
        let mut loads = false;
        let mut stores = false;
        while let Some(b) = work.pop() {
            if !visited.insert(b) {
                continue;
            }
            if b == header || b == wait {
                return None;
            }
            let block = &program.blocks[b];
            let mut done = false;
            for (i, inst) in block.insts.iter().enumerate() {
                let name = inst.name();
                if name == "s_sendmsg" && matches!(inst.fields, Fields::Sopp { imm: DONE }) {
                    ends.insert((b, i));
                    done = true;
                    break;
                }
                // A discarded pixel can terminate without sending the ordered
                // completion message. The terminal null export writes no color.
                if matches!(
                    inst.fields,
                    Fields::Exp {
                        target: 9,
                        done: true,
                        src: [None, None, None, None],
                        ..
                    }
                ) && block.exit == Exit::End
                    && block.insts[i + 1..]
                        .iter()
                        .all(|i| i.name().starts_with("s_endpgm"))
                {
                    done = true;
                    break;
                }
                match inst.fields {
                    Fields::Image { dim: 1, .. }
                        if matches!(name, "image_load" | "image_store") =>
                    {
                        images.insert((b, i));
                        loads |= name == "image_load";
                        stores |= name == "image_store";
                    }
                    Fields::Smem { .. }
                        if name.starts_with("s_load_") || name.starts_with("s_buffer_load_") => {}
                    Fields::Image { .. }
                    | Fields::Buffer { .. }
                    | Fields::Ds { .. }
                    | Fields::Flat { .. }
                    | Fields::Exp { .. }
                    | Fields::Smem { .. } => return None,
                    Fields::Sopp { .. } if name == "s_sendmsg" => return None,
                    _ => {}
                }
            }
            if !done {
                let successors = block.exit.successors();
                if successors.is_empty() {
                    return None;
                }
                work.extend(successors);
            }
        }
        (loads && stores && ends.len() == 1).then_some(Self {
            overlap,
            polls: [first.machine.offset, repeated.machine.offset],
            images,
            branches: [header, wait],
            resume,
        })
    }
}

fn reads_exit_wave(inst: &Inst) -> bool {
    matches!(&inst.fields, Fields::Sop { src, .. } if src.contains(&Operand::Const(EXITING_WAVE)))
}

fn overlap_fields(program: &Program, value: Value) -> bool {
    let mut packer = false;
    let mut overlaps = false;
    for inst in program.blocks.iter().flat_map(|b| &b.insts) {
        if !inst.reads.iter().any(|r| r.1 == value) {
            continue;
        }
        let Fields::Sop { src, .. } = &inst.fields else {
            return false;
        };
        match (inst.name(), src.get(1)) {
            ("s_bfe_u32", Some(Operand::Literal(0x0002_001c))) => packer = true,
            ("s_bfe_u32", Some(Operand::Literal(0x000a_0000 | 0x000a_0010))) => {}
            ("s_lshr_b32", Some(Operand::Const(n))) if inline(*n) == Some(31) => overlaps = true,
            _ => return false,
        }
    }
    packer && overlaps
}

/// The console-only values may feed the wait and its register configuration.
/// Reject them if any data or execution-mask use escapes that control flow.
fn wait_uses_only(
    program: &Program,
    overlap: Value,
    first: &Inst,
    repeated: &Inst,
    header: usize,
    wait: usize,
) -> bool {
    let mut pending = vec![overlap];
    pending.extend(first.writes.iter().chain(&repeated.writes).map(|w| w.1));
    let mut seen = HashSet::new();
    while let Some(value) = pending.pop() {
        if !seen.insert(value) {
            continue;
        }
        for (b, block) in program.blocks.iter().enumerate() {
            if block.phis.iter().any(|p| p.args.contains(&value)) {
                return false;
            }
            for inst in &block.insts {
                if !inst.reads.iter().any(|r| r.1 == value) {
                    continue;
                }
                let safe = matches!(
                    inst.name(),
                    "s_bfe_u32"
                        | "s_lshr_b32"
                        | "s_lshl_b32"
                        | "s_or_b32"
                        | "s_or_b64"
                        | "s_not_b32"
                        | "s_add_i32"
                        | "s_cmp_eq_u32"
                        | "s_cmp_gt_u32"
                        | "s_cmp_lt_u32"
                        | "s_setreg_b32"
                        | "s_cselect_b32"
                        | "s_cselect_b64"
                        | "s_and_b32"
                        | "s_and_b64"
                        | "s_cbranch_vccnz"
                        | "s_cbranch_scc0"
                );
                if (b != header && b != wait)
                    || !safe
                    || (inst.name() == "s_setreg_b32"
                        && !matches!(
                            inst.fields,
                            Fields::Sop {
                                imm: Some(PACKER),
                                ..
                            }
                        ))
                    || inst
                        .writes
                        .iter()
                        .any(|w| matches!(w.0, Reg::S(EXEC..=127)))
                {
                    return false;
                }
                pending.extend(inst.writes.iter().map(|w| w.1));
            }
        }
    }
    true
}

impl Lower<'_> {
    pub(super) fn ordered_image(&self, site: Site) -> bool {
        self.ordered_pixel
            .as_ref()
            .is_some_and(|p| p.images.contains(&site))
    }

    pub(super) fn ordered_shape(&mut self, site: Site, shape: u32, uav: bool) -> Result<u32, Why> {
        if !self.ordered_image(site) {
            return Ok(shape);
        }
        if !uav {
            return Err("an ordered pixel load with no unordered access view");
        }
        self.s.flags |= flags::ROVS;
        Ok(shape | ROV)
    }

    pub(super) fn ordered_branch(
        &mut self,
        block: usize,
        taken: usize,
        not_taken: usize,
    ) -> Option<V> {
        let p = self.ordered_pixel.as_ref()?;
        // After replacing the wait with ROV accesses, go directly to the
        // ordered body even when every console lane in the wave is inactive.
        (p.branches.contains(&block) && [taken, not_taken].contains(&p.resume))
            .then(|| self.s.m.bool(taken == p.resume))
    }
}
