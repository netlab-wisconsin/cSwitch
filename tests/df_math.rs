use scx_rustland_la::df_ccm_sampler::{beats_to_mib_s_x100, pressure_from_bw_x100};

#[test]
fn beat_deltas_convert_to_bandwidth() {
    let one_mib_in_10ms = 32_768;
    assert_eq!(beats_to_mib_s_x100(one_mib_in_10ms, 32, 0.01), 10_000);

    let half_mib_in_10ms = 8_192;
    assert_eq!(beats_to_mib_s_x100(half_mib_in_10ms, 64, 0.01), 5_000);
}

#[test]
fn pressure_uses_max_read_or_write_bw() {
    assert_eq!(
        pressure_from_bw_x100(3_300_000, 1_000_000, 3_300_000),
        10_000
    );
    assert_eq!(
        pressure_from_bw_x100(1_650_000, 1_000_000, 3_300_000),
        8_030
    );
    assert_eq!(pressure_from_bw_x100(100_000, 250_000, 3_300_000), 1_060);
}
