/// RAM of the computer, in MiB (the same numbers `src/main/memory.ts` reports).
pub fn system_memory() -> (u64, u64) {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    (system.total_memory() / 1_048_576, system.available_memory() / 1_048_576)
}
