//! Fuzzes length-prefixed access-unit inspection and parameter-set stripping.
#![no_main]

use iris_h265::{inspect_access_unit, strip_parameter_sets};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = inspect_access_unit(data);
    let _ = strip_parameter_sets(data);
});
