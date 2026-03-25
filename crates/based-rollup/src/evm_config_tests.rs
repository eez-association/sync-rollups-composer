use alloy_primitives::B256;

#[test]
fn test_set_context_calldata_with_extreme_values() {
    use crate::payload_builder::{L1BlockInfo, encode_set_context_calldata};

    let l1_info = L1BlockInfo {
        l1_block_number: u64::MAX,
        l1_block_hash: B256::repeat_byte(0xFF),
    };
    let calldata = encode_set_context_calldata(&l1_info);
    // Should encode without panic; 4 bytes selector + 2 * 32 bytes args = 68 bytes
    assert_eq!(calldata.len(), 68);
}
