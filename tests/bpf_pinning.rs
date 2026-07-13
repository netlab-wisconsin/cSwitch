fn rustland_tick_body(source: &str) -> &str {
    let start = source
        .find("void BPF_STRUCT_OPS(rustland_tick")
        .expect("rustland_tick function");
    let end = source[start..]
        .find("void BPF_STRUCT_OPS(rustland_update_idle")
        .map(|offset| start + offset)
        .expect("function after rustland_tick");
    &source[start..end]
}

#[test]
fn tick_fastpaths_single_cpu_affinity_before_userspace_trigger() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    for relative in [
        "vendor/scx_rustland_core/assets/bpf/main.bpf.c",
        "main.bpf.c",
    ] {
        let path = std::path::Path::new(manifest_dir).join(relative);
        let source = std::fs::read_to_string(&path).expect("read BPF source");
        let body = rustland_tick_body(&source);
        let guard = body
            .find("p->nr_cpus_allowed == 1")
            .unwrap_or_else(|| panic!("{relative}: missing pinned-task tick guard"));
        let userspace_trigger = body
            .find("tctx->pending_trigger = QUEUE_TRIGGER_TICK")
            .unwrap_or_else(|| panic!("{relative}: missing tick userspace trigger"));

        assert!(
            guard < userspace_trigger,
            "{relative}: pinned-task guard must run before tick userspace trigger"
        );
        let guarded_block = &body[guard..userspace_trigger];
        assert!(
            guarded_block.contains("__sync_fetch_and_add(&nr_tick_fastpath_stay, 1);"),
            "{relative}: pinned-task guard should count as tick fastpath stay"
        );
        assert!(
            guarded_block.contains("return;"),
            "{relative}: pinned-task guard must not fall through to userspace trigger"
        );
    }
}
