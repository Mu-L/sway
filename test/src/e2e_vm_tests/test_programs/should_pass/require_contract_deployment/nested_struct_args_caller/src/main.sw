script;

use nested_struct_args_abi::*;

#[cfg(experimental_new_encoding = false)]
const CONTRACT_ID = 0xe63d33a1b3a6903808b379f6a41a72fa8a370e8b76626775e7d9d2f9c4c5da40;
#[cfg(experimental_new_encoding = true)]
const CONTRACT_ID = 0x7f7028de5ba246cf26c182536631f74230f5d6e3ab99df370e85ba749e2271b9; // AUTO-CONTRACT-ID ../../test_contracts/nested_struct_args_contract --release

fn main() -> bool {
    let caller = abi(NestedStructArgs, CONTRACT_ID);

    let param_one = StructOne {
        inn: Inner { foo: 42 },
    };
    let param_two = StructTwo { foo: 42 };

    assert(caller.foo(param_one, param_two) == 85);
    true
}
