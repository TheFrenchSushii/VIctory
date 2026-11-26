// Synthetic GFX10 fixture from DoubleGhosty/cool_fox (GPL-3.0-or-later).

use super::container::{self, SigElement};
use super::{
    Container, FourCC, HwStage, NativeProgram, RootType, ShaderError, emit, io, ir, resource,
    structure,
};
use crate::native::StageIo;

/// A pixel program with an ordered section, as the console compiler
/// makes one of rasterizer ordered views (the title's hair cards keep
/// their nearest layers that way): the wait for overlapped waves is
/// skipped, the views inside the section are the host's ordered ones,
/// the one after it is not.
#[test]
fn an_ordered_section_uses_rasterizer_ordered_views() {
    let section = [
        0xBE82_0300u32, // s_mov_b32 s2, s0
        0xBE83_0380,    // s_mov_b32 s3, 0
        0xF40C_0201,    // s_load_dwordx8 s[8:15], s[2:3], 0
        0xFA00_0000,
        0x9390_FF01, // s_bfe_u32 s16, s1, 0x2001c
        0x0002_001C,
        0x9391_FF01, // s_bfe_u32 s17, s1, 0xa0000
        0x000A_0000,
        0x8F10_8110, // s_lshl_b32 s16, s16, 1
        0x9392_FF01, // s_bfe_u32 s18, s1, 0xa0010
        0x000A_0010,
        0xBE91_0711, // s_not_b32 s17, s17
        0x9013_9F01, // s_lshr_b32 s19, s1, 31
        0x8810_8110, // s_or_b32 s16, s16, 1
        0x8112_1112, // s_add_i32 s18, s18, s17
        0xBF06_8013, // s_cmp_eq_u32 s19, 0
        0xB990_1019, // s_setreg_b32 hwreg(HW_REG_POPS_PACKER, 0, 3), s16
        0x8514_807E, // s_cselect_b32 s20, exec_lo, 0
        0x8115_11EF, // s_add_i32 s21, src_pops_exiting_wave_id, s17
        0xBF08_1215, // s_cmp_gt_u32 s21, s18
        0x8516_807E, // s_cselect_b32 s22, exec_lo, 0
        0x8814_1614, // s_or_b32 s20, s20, s22
        0x876A_147E, // s_and_b32 vcc_lo, exec_lo, s20
        0xBF87_0004, // s_cbranch_vccnz past the wait
        0xBF8E_FFFF, // s_sleep 0xffff
        0x8115_11EF, // s_add_i32 s21, src_pops_exiting_wave_id, s17
        0xBF0A_1512, // s_cmp_lt_u32 s18, s21
        0xBF84_FFFC, // s_cbranch_scc0 back to the sleep
        0x7E00_0280, // v_mov_b32 v0, 0
        0x7E02_0280, // v_mov_b32 v1, 0
        0xF000_3F88, // image_load v[2:5], v[0:1], s[8:15] dmask:0xf 2D glc dlc
        0x0002_0200,
        0x0604_04F2, // v_add_f32 v2, 1.0, v2
        0xF020_3F08, // image_store v[2:5], v[0:1], s[8:15] dmask:0xf 2D glc
        0x0002_0200,
        0xBEFC_0380, // s_mov_b32 m0, 0
        0xBF90_0007, // s_sendmsg sendmsg(MSG_ORDERED_PS_DONE)
    ];
    let end = [
        0xF800_180Fu32, // exp mrt0 v2, v2, v2, v2 done vm
        0x0202_0202,
        0xBF81_0000, // s_endpgm
    ];
    // A store after the section, through an ordinary view.
    let after = [0xF020_3F08u32, 0x0002_0200];
    let lower = |words: Vec<u32>| {
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 8,
            layout: vec![crate::native::RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![crate::native::UsedResource {
                kind: crate::native::ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 8,
                user_data: 0,
                slot: 0,
            }],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let target = SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        };
        let osg1 = container::signature(&[target], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
        let st = structure::structure(&p)?;
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native)?;
        let out = emit(&c, &native, &p, &st, &io, &res)?;
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        Ok::<u64, ShaderError>(u64::from_le_bytes(sfi0.try_into().unwrap()))
    };
    // D3D_SHADER_REQUIRES_ROVS
    let features = lower([&section[..], &end].concat()).unwrap();
    assert_ne!(features & 0x1000, 0, "SFI0 {features:#x}: no ROVs");
    lower([&section[..], &after, &end].concat()).unwrap();
    // The ordered body can also terminate a discarded pixel without sending
    // MSG_ORDERED_PS_DONE. A color export on that path is outside this protocol.
    let jump = [0xBF88_0000 | (section.len() - 28 + end.len()) as u32]; // s_cbranch_execz
    let null_end = [0xF800_1890, 0, 0xBF81_0000];
    let exits_at =
        |terminal: &[u32]| [&section[..28], &jump, &section[28..], &end, terminal].concat();
    assert_ne!(lower(exits_at(&null_end)).unwrap() & 0x1000, 0);
    assert!(lower(exits_at(&end)).is_err());
    // The same accesses outside a section: ordinary views, no feature.
    let plain = [&section[..4], &section[28..35], &end].concat();
    let features = lower(plain).unwrap();
    assert_eq!(features & 0x1000, 0, "SFI0 {features:#x}: ROVs");
    // Without the section's start the wait stays unknown.
    let mut unmarked = [&section[..], &end].concat();
    unmarked.remove(16);
    assert!(lower(unmarked).is_err());
}
