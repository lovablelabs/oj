use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct ParseFlags: u32 {
        const None = 0;
        const Yield = 1 << 0;
        const Await = 1 << 1;
        const Type = 1 << 2;
        const IgnoreMissingOpenBrace = 1 << 4;
        const JSDoc = 1 << 5;
    }
}
