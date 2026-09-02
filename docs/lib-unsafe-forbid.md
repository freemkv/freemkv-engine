# Why `#![forbid(unsafe_code)]`

No reason for this crate to use `unsafe`; a prior test helper's SAFETY note
justified the wrong condition for `std::env::set_var` (env access must be
single-threaded process-wide). `forbid`, not `deny`, so it can't be
re-allowed.
