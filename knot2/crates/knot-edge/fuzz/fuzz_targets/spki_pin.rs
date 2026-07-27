#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    knot_edge::fuzz::spki_pin(data);
});
