use scx_rustland_la::topology::parse_l3_size_mb_text;

#[test]
fn parse_l3_size_text_supports_common_units() {
    assert_eq!(parse_l3_size_mb_text("32768K\n"), 32.0);
    assert_eq!(parse_l3_size_mb_text("32M"), 32.0);
    assert_eq!(parse_l3_size_mb_text("1G"), 1024.0);
    assert_eq!(parse_l3_size_mb_text("48"), 48.0);
}
