fn main() -> anyhow::Result<()> {
    #[cfg(feature = "stall-filler-spinner")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(
            scx_rustland_la::STALL_FILLER_SPINNER_ARG,
        ))
    {
        return run_stall_filler_spinner();
    }
    scx_rustland_la::run()
}

#[cfg(feature = "stall-filler-spinner")]
fn run_stall_filler_spinner() -> anyhow::Result<()> {
    let name = std::ffi::CString::new("rustland_filler")?;
    let _ = unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0, 0, 0) };

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    loop {
        for _ in 0..1024 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            std::hint::black_box(state);
        }
        std::hint::spin_loop();
    }
}
