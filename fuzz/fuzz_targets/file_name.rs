#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| hiroi_engine::fuzzing::file_name(data));
