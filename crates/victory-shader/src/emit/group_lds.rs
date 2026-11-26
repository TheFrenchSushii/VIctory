use super::{Attr, Fields, Inst, LaneExit, Lower, Structured, host_max_wave, meeting_mode};

pub(super) fn access(inst: &Inst) -> bool {
    matches!(inst.fields, Fields::Ds { .. })
        && !(inst.name().starts_with("ds_swizzle") || inst.name().contains("permute"))
}

/// One thread cannot exchange LDS data with another lane. For larger groups that
/// fit in one guest wave, support a smaller host wave if every thread reaches the
/// same LDS instructions in the same order. A straight-line program gives that
/// guarantee even when individual accesses are masked off by EXEC.
pub(super) fn portable(group: u32, wave: u32, st: &Structured) -> bool {
    group == 1 || ((1..wave).contains(&host_max_wave()) && matches!(&st.exits[..], [LaneExit::End]))
}

impl Lower<'_> {
    pub(super) fn sync_lds(&mut self, inst: &Inst, block: usize) {
        if !access(inst) {
            return;
        }
        // Wait outside the instruction's EXEC guard. Earlier writes must be
        // visible to subsequent readers, and earlier reads must finish before
        // later writes can overwrite their data.
        let mode = if self.portable_lds && self.s.threads.is_some_and(|t| t != [1, 1, 1]) {
            1 | 8
        } else if self.split_lds && self.lds_meet.as_ref().is_none_or(|(all, _)| all[block]) {
            meeting_mode()
        } else {
            return;
        };
        let mode = self.s.m.i32(mode);
        let void = self.s.t.void;
        self.s
            .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }
}
