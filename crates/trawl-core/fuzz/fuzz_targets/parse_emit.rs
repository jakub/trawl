#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    if let Ok(query) = trawl_core::parser::parse(data) {
        let _ = trawl_core::emitter::emit(&query, "test.parquet");
    }
});
