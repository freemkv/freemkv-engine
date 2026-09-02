# Why `Progress::eta_secs`/`speed_bps` are engine-computed, not front-end-derived

`speed_bps` and `eta_secs` on `Progress` are computed ONCE by the engine
(one smoothing algorithm, one remaining-bytes/speed formula) and never
re-derived per front-end.

This exists because three independent ETA implementations were found in
the pre-split tree, all disagreeing: `autorip/src/mover.rs` computed
`remaining_gb*1024/speed_mbs` and baked it straight into an `m:ss` string
with no hour-carry; `autorip/src/ripper/mux.rs` received an already-
formatted `eta: String` from yet another call site; the CLI's `fmt_eta`
took raw seconds and did its own `h:mm:ss` formatting. Three algorithms,
not just three renderers — exactly the drift class that let a real
`key_fetch` bug happen (§2 of the design doc): the same computation
answered twice, and the two answers disagreed.

A front-end may still format `eta_secs`/`speed_bps` however it likes
(`"1:23"` vs `"0:01:23"`, MB/s vs Mb/s) — that's real presentation choice
with no correctness content. What must NOT happen is a front-end
recomputing the ETA from raw byte deltas itself.
