use std::cell::Cell;
thread_local! {
    pub static FLAT_MODE: Cell<Option<bool>> = const { Cell::new(None) };
    pub static FLAT_BUDGET: Cell<Option<u64>> = const { Cell::new(None) };
}
