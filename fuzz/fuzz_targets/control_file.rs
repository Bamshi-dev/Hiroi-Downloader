#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| hiroi_engine::fuzzing::control_file(data));
