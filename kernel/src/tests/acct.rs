use ktest::ktest;

#[ktest]
fn accounting_encodes_comp_t_and_elapsed_float_without_fpu() {
    assert_eq!(crate::acct::acct_v3_version(), 3);

    assert_eq!(crate::acct::encode_comp_t(0), 0);
    assert_eq!(crate::acct::encode_comp_t(0x1fff), 0x1fff);
    assert_eq!(crate::acct::encode_comp_t(0x2000), 0x2400);
    assert_eq!(crate::acct::encode_comp_t(u64::MAX), 0xffff);

    assert_eq!(crate::acct::ns_to_f32_bits(0), 0);
    assert_eq!(crate::acct::ns_to_f32_bits(1_000_000_000), 0x3f80_0000);
    assert_eq!(crate::acct::ns_to_f32_bits(1_500_000_000), 0x3fc0_0000);
    assert_eq!(crate::acct::ns_to_f32_bits(100_000_000), 0x3dcc_cccd);
}
