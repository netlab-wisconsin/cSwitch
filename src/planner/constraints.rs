#[derive(Clone, Debug)]
pub(super) enum SyncConstraint {
    CoLocateGroup { tgid: u32, tids: Vec<u32> },
    ResidualPair { left: u32, right: u32 },
}
