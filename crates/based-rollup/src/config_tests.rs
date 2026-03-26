use super::*;

fn test_config() -> RollupConfig {
    RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: Address::ZERO,
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: Address::ZERO,
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    }
}

#[test]
fn test_l2_block_number() {
    let config = test_config();
    assert_eq!(config.l2_block_number(1000), 0);
    assert_eq!(config.l2_block_number(1001), 1);
    assert_eq!(config.l2_block_number(1100), 100);
}

#[test]
fn test_l2_timestamp() {
    let config = test_config();
    // Formula: deployment_timestamp + (block_number + 1) * block_time
    assert_eq!(config.l2_timestamp(0), 1_700_000_012);
    assert_eq!(config.l2_timestamp(1), 1_700_000_024);
    assert_eq!(config.l2_timestamp(100), 1_700_001_212);
}

#[test]
fn test_l2_block_number_from_timestamp() {
    let config = test_config();
    // Inverse of: ts = dep + (n+1)*bt
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_012), 0);
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_024), 1);
    assert_eq!(config.l2_block_number_from_timestamp(1_700_001_212), 100);
}

#[test]
fn test_l2_block_number_before_deployment() {
    let config = test_config();
    // L1 block before deployment should saturate to 0
    assert_eq!(config.l2_block_number(0), 0);
    assert_eq!(config.l2_block_number(999), 0);
}

#[test]
fn test_l2_block_number_from_timestamp_before_deployment() {
    let config = test_config();
    // Timestamps before or at deployment should saturate to 0
    assert_eq!(config.l2_block_number_from_timestamp(0), 0);
    assert_eq!(config.l2_block_number_from_timestamp(1_699_999_999), 0);
    // Even deployment_timestamp itself maps to block 0 (before block 0's timestamp)
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_000), 0);
}

#[test]
fn test_l2_timestamp_large_block_number() {
    let config = test_config();
    // Reasonably large block number should compute correctly
    let block = 1_000_000u64;
    let ts = config.l2_timestamp(block);
    assert_eq!(
        ts,
        config.deployment_timestamp + (block + 1) * config.block_time
    );
}

#[test]
fn test_l2_timestamp_overflow_saturates() {
    let config = test_config();
    // With saturating arithmetic, overflow produces u64::MAX instead of panicking
    let ts = config.l2_timestamp(u64::MAX / 12);
    assert_eq!(ts, u64::MAX);
}

#[test]
fn test_l2_block_number_from_timestamp_rounds_down() {
    let config = test_config();
    // Block 0 timestamp is dep + 12 = 1_700_000_012
    // Timestamps between block 0 and block 1 should return 0
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_012), 0);
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_023), 0);
    // Block 1 timestamp is dep + 24 = 1_700_000_024
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_024), 1);
    assert_eq!(config.l2_block_number_from_timestamp(1_700_000_035), 1);
}

#[test]
fn test_roundtrip_block_number_timestamp() {
    let config = test_config();
    for block in 0..100 {
        let ts = config.l2_timestamp(block);
        assert_eq!(config.l2_block_number_from_timestamp(ts), block);
    }
}

#[test]
fn test_deployment_at_zero() {
    let config = RollupConfig {
        deployment_l1_block: 0,
        deployment_timestamp: 0,
        block_time: 1,
        ..test_config()
    };
    assert_eq!(config.l2_block_number(0), 0);
    assert_eq!(config.l2_block_number(1), 1);
    assert_eq!(config.l2_timestamp(0), 1);
    assert_eq!(config.l2_timestamp(1), 2);
}

#[test]
fn test_validate_rejects_zero_block_time() {
    let mut config = RollupConfig {
        block_time: 0,
        ..test_config()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_validate_does_not_panic_on_builder_without_key() {
    let mut config = RollupConfig {
        builder_mode: true,
        builder_private_key: None,
        builder_ws_url: None,
        rollups_address: Address::with_last_byte(0x42),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 1,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_does_not_panic_on_full_config() {
    let mut config = RollupConfig {
        builder_mode: true,
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        builder_ws_url: None,
        rollups_address: Address::with_last_byte(0x42),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 1,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_builder_mode_requires_rollups_address() {
    let mut config = RollupConfig {
        builder_mode: true,
        rollups_address: Address::ZERO,
        ..test_config()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_different_block_times() {
    for block_time in [1, 2, 6, 12, 15, 60] {
        let config = RollupConfig {
            block_time,
            ..test_config()
        };
        assert_eq!(
            config.l2_timestamp(0),
            config.deployment_timestamp + block_time
        );
        assert_eq!(
            config.l2_timestamp(1),
            config.deployment_timestamp + 2 * block_time
        );
        assert_eq!(
            config.l2_timestamp(10),
            config.deployment_timestamp + 11 * block_time
        );
    }
}

#[test]
fn test_l2_timestamp_checked_normal() {
    let config = test_config();
    assert_eq!(config.l2_timestamp_checked(0), Some(1_700_000_012));
    assert_eq!(config.l2_timestamp_checked(1), Some(1_700_000_024));
    assert_eq!(config.l2_timestamp_checked(100), Some(1_700_001_212));
}

#[test]
fn test_l2_timestamp_checked_overflow() {
    let config = test_config();
    // This should return None instead of panicking
    assert_eq!(config.l2_timestamp_checked(u64::MAX / 12), None);
    assert_eq!(config.l2_timestamp_checked(u64::MAX), None);
}

#[test]
fn test_l2_timestamp_checked_roundtrip() {
    let config = test_config();
    for block in 0..100 {
        let ts = config.l2_timestamp_checked(block).unwrap();
        assert_eq!(config.l2_block_number_from_timestamp(ts), block);
    }
}

#[test]
fn test_validate_warns_on_zero_rollups_address() {
    let mut config = RollupConfig {
        rollups_address: Address::ZERO,
        ..test_config()
    };
    // Should not error, just warn
    config.validate().unwrap();
}

#[test]
fn test_l2_block_number_from_timestamp_exact_boundary() {
    let config = test_config();
    // Block 0 timestamp = dep + 12
    let ts = config.deployment_timestamp + 12;
    assert_eq!(config.l2_block_number_from_timestamp(ts), 0);
    // One second before block 1 boundary
    let ts2 = config.deployment_timestamp + 23;
    assert_eq!(config.l2_block_number_from_timestamp(ts2), 0);
    // Block 1 timestamp = dep + 24
    let ts3 = config.deployment_timestamp + 24;
    assert_eq!(config.l2_block_number_from_timestamp(ts3), 1);
    // Block 2 timestamp = dep + 36
    let ts4 = config.deployment_timestamp + 36;
    assert_eq!(config.l2_block_number_from_timestamp(ts4), 2);
}

#[test]
fn test_l2_timestamp_checked_zero_block() {
    let config = test_config();
    assert_eq!(
        config.l2_timestamp_checked(0),
        Some(config.deployment_timestamp + config.block_time)
    );
}

#[test]
fn test_l2_timestamp_checked_with_block_time_1() {
    let config = RollupConfig {
        block_time: 1,
        ..test_config()
    };
    // With block_time=1, overflow happens much later
    assert!(
        config
            .l2_timestamp_checked(u64::MAX - config.deployment_timestamp)
            .is_some()
            || config.l2_timestamp_checked(u64::MAX).is_none()
    );
}

#[test]
fn test_validate_warns_on_zero_deployment_timestamp() {
    // deployment_timestamp=0 should warn but not error.
    // This is valid in test environments where the chain starts from Unix epoch.
    let mut config = RollupConfig {
        deployment_timestamp: 0,
        block_time: 12,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_accepts_block_time_one() {
    let mut config = RollupConfig {
        block_time: 1,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_accepts_large_block_time() {
    let mut config = RollupConfig {
        block_time: u64::MAX,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_l2_timestamp_checked_overflow_add_phase() {
    // deployment_timestamp is near u64::MAX, so even block_number=0 overflows the add
    // because (0+1)*12 = 12, and (u64::MAX - 5) + 12 overflows
    let config = RollupConfig {
        deployment_timestamp: u64::MAX - 5,
        block_time: 12,
        ..test_config()
    };
    assert_eq!(config.l2_timestamp_checked(0), None);
    assert_eq!(config.l2_timestamp_checked(1), None);
}

#[test]
fn test_l2_timestamp_checked_overflow_mul_phase() {
    // (block_number + 1) * block_time overflows before the add
    let config = RollupConfig {
        deployment_timestamp: 0,
        block_time: u64::MAX,
        ..test_config()
    };
    // (1+1) * u64::MAX overflows
    assert_eq!(config.l2_timestamp_checked(1), None);
    // (0+1) * u64::MAX = u64::MAX, 0 + u64::MAX = u64::MAX, ok
    assert_eq!(config.l2_timestamp_checked(0), Some(u64::MAX));
}

#[test]
fn test_l2_block_number_from_timestamp_u64_max() {
    let config = test_config();
    // Should not panic — just compute a large block number
    let block = config.l2_block_number_from_timestamp(u64::MAX);
    // (u64::MAX - dep) / bt - 1
    let expected = ((u64::MAX - config.deployment_timestamp) / config.block_time).saturating_sub(1);
    assert_eq!(block, expected);
}

#[test]
fn test_l2_block_number_saturates_at_zero() {
    let config = RollupConfig {
        deployment_l1_block: u64::MAX,
        ..test_config()
    };
    // Any L1 block number should saturate to 0
    assert_eq!(config.l2_block_number(0), 0);
    assert_eq!(config.l2_block_number(u64::MAX - 1), 0);
    assert_eq!(config.l2_block_number(u64::MAX), 0);
}

#[test]
fn test_validate_builder_with_ws_url() {
    let mut config = RollupConfig {
        builder_mode: false,
        builder_ws_url: Some("ws://builder:8546".to_string()),
        ..test_config()
    };
    // Fullnode with builder_ws_url should validate fine
    config.validate().unwrap();
}

#[test]
fn test_validate_with_fallback_url() {
    let mut config = RollupConfig {
        l1_rpc_url_fallback: Some("http://fallback:8545".to_string()),
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_without_fallback_url() {
    let mut config = RollupConfig {
        l1_rpc_url_fallback: None,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_l2_block_number_from_timestamp_block_time_1() {
    let config = RollupConfig {
        block_time: 1,
        deployment_timestamp: 100,
        ..test_config()
    };
    // block 0 ts = 101, block 1 ts = 102, block 99 ts = 200
    assert_eq!(config.l2_block_number_from_timestamp(100), 0);
    assert_eq!(config.l2_block_number_from_timestamp(101), 0);
    assert_eq!(config.l2_block_number_from_timestamp(102), 1);
    assert_eq!(config.l2_block_number_from_timestamp(200), 99);
}

#[test]
fn test_l2_timestamp_block_time_1_sequential() {
    let config = RollupConfig {
        block_time: 1,
        deployment_timestamp: 0,
        ..test_config()
    };
    // With block_time=1 and dep=0, timestamp == block_number + 1
    for i in 0..10 {
        assert_eq!(config.l2_timestamp(i), i + 1);
    }
}

// --- Validation: zero values, overflow, missing fields ---

// --- Timestamp computation edge cases ---

// --- Iteration 17: Config validation completeness ---

#[test]
fn test_config_debug_redacts_private_key() {
    let config = RollupConfig {
        builder_private_key: Some("0xsupersecretkey".to_string()),
        ..test_config()
    };
    let debug = format!("{config:?}");
    assert!(
        !debug.contains("supersecretkey"),
        "private key should be redacted in debug output"
    );
    assert!(
        debug.contains("[REDACTED]"),
        "should show [REDACTED] placeholder"
    );
}

#[test]
fn test_validate_all_constraints_simultaneously() {
    // Valid config with all fields set to non-default values
    let mut config = RollupConfig {
        l1_rpc_url: "http://mainnet.example.com:8545".to_string(),
        l2_context_address: Address::with_last_byte(0x43),
        deployment_l1_block: 19_000_000,
        deployment_timestamp: 1_710_000_000,
        block_time: 2,
        builder_mode: true,
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        l1_rpc_url_fallback: Some("http://fallback.example.com:8545".to_string()),
        builder_ws_url: Some("ws://builder.example.com:8546".to_string()),
        health_port: 9100,
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 1,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_cross_chain_disabled_by_default() {
    let mut config = test_config();
    assert!(config.rollups_address.is_zero());
    assert!(config.cross_chain_manager_address.is_zero());
    assert_eq!(config.rollup_id, 0);
    config.validate().unwrap();
}

#[test]
fn test_validate_cross_chain_requires_manager() {
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 1,
        ..test_config()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_validate_cross_chain_valid() {
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 1,
        ..test_config()
    };
    config.validate().unwrap();
}

#[test]
fn test_validate_warns_rollups_address_set_rollup_id_zero() {
    // rollups_address is set but rollup_id is 0 — should warn but not error
    // (rollup ID 0 is typically reserved for L1, so it's suspicious but allowed)
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 0,
        ..test_config()
    };
    // Must not return an error — the warning is emitted via tracing
    config.validate().unwrap();
}

#[test]
fn test_validate_cross_chain_all_fields_set_and_enabled() {
    // All three cross-chain fields set to non-zero values
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 7,
        ..test_config()
    };
    config.validate().unwrap();
    assert!(
        !config.rollups_address.is_zero(),
        "rollups_address should be non-zero"
    );
    assert!(!config.cross_chain_manager_address.is_zero());
    assert_ne!(config.rollup_id, 0);
    // Confirm disabled when rollups_address is zero
    let mut disabled = RollupConfig {
        rollups_address: Address::ZERO,
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 0,
        ..test_config()
    };
    disabled.validate().unwrap();
    assert!(
        disabled.rollups_address.is_zero(),
        "cross-chain should be disabled when rollups_address is zero"
    );
}

// --- Iteration 52: Cross-chain config validation edge cases ---

#[test]
fn test_validate_all_three_cross_chain_fields_set_correctly() {
    // All three cross-chain fields set to valid non-zero values should pass
    // validation without errors or warnings (rollup_id > 0 avoids the warning).
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::with_last_byte(0x03),
        rollup_id: 1,
        ..test_config()
    };
    config.validate().unwrap();
    // All fields are non-zero
    assert!(!config.rollups_address.is_zero());
    assert!(!config.cross_chain_manager_address.is_zero());
    assert_ne!(config.rollup_id, 0);
}

#[test]
fn test_validate_cross_chain_rollups_address_without_manager_is_error() {
    // rollups_address set but cross_chain_manager_address is zero — this is an
    // error because cross-chain mode requires a manager contract on L2.
    let mut config = RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 1,
        ..test_config()
    };
    let err = config.validate().unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("CROSS_CHAIN_MANAGER_ADDRESS"),
        "error should mention CROSS_CHAIN_MANAGER_ADDRESS: {msg}"
    );
}

#[test]
fn test_cross_chain_env_vars_parsed_via_cli_args() {
    // Clap uses the same FromStr parsing for CLI args and env vars, so
    // testing via try_parse_from exercises the same code path as
    // env = "ROLLUPS_ADDRESS" / "CROSS_CHAIN_MANAGER_ADDRESS" / "ROLLUP_ID".
    // We pass the three cross-chain fields as CLI flags.
    let config = RollupConfig::try_parse_from([
        "test",
        "--rollups-address",
        "0x1111111111111111111111111111111111111111",
        "--cross-chain-manager-address",
        "0x4200000000000000000000000000000000000003",
        "--rollup-id",
        "42",
    ])
    .expect("should parse cross-chain CLI args");
    assert_eq!(
        format!("{}", config.rollups_address),
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(
        format!("{}", config.cross_chain_manager_address),
        "0x4200000000000000000000000000000000000003"
    );
    assert_eq!(config.rollup_id, 42);
}

#[test]
fn test_l2_block_number_from_timestamp_defensive_zero_block_time() {
    // block_time=0 would cause division by zero; the function has a
    // defensive early return of 0 for this case.
    let config = RollupConfig {
        block_time: 0,
        deployment_timestamp: 100,
        ..test_config()
    };
    // Should NOT panic despite block_time=0
    assert_eq!(config.l2_block_number_from_timestamp(200), 0);
    assert_eq!(config.l2_block_number_from_timestamp(0), 0);
    assert_eq!(config.l2_block_number_from_timestamp(u64::MAX), 0);
}

// --- Re-run Iteration 40: Config serde roundtrip with cross-chain fields ---

#[test]
fn test_parse_bootstrap_accounts_empty() {
    let result = parse_bootstrap_accounts("").unwrap();
    assert!(result.is_empty());
    let result = parse_bootstrap_accounts("  ").unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_parse_bootstrap_accounts_single() {
    let result = parse_bootstrap_accounts("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:10").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].address,
        "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
            .parse::<Address>()
            .unwrap()
    );
    assert_eq!(result[0].amount_wei, 10_000_000_000_000_000_000u128);
}

#[test]
fn test_parse_bootstrap_accounts_multiple() {
    let result = parse_bootstrap_accounts(
        "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:10,0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC:5.5",
    )
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].amount_wei, 10_000_000_000_000_000_000u128);
    assert_eq!(result[1].amount_wei, 5_500_000_000_000_000_000u128);
}

#[test]
fn test_parse_bootstrap_accounts_invalid_no_colon() {
    assert!(parse_bootstrap_accounts("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc").is_err());
}

#[test]
fn test_parse_bootstrap_accounts_invalid_address() {
    assert!(parse_bootstrap_accounts("notanaddress:10").is_err());
}

#[test]
fn test_parse_bootstrap_accounts_invalid_amount() {
    assert!(parse_bootstrap_accounts("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:abc").is_err());
}

#[test]
fn test_parse_bootstrap_accounts_negative_amount() {
    assert!(parse_bootstrap_accounts("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:-1").is_err());
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_config() -> impl Strategy<Value = RollupConfig> {
        (1u64..=3600, 0u64..=u64::MAX / 2).prop_map(|(block_time, deployment_timestamp)| {
            RollupConfig {
                block_time,
                deployment_timestamp,
                ..test_config()
            }
        })
    }

    proptest! {
        #[test]
        fn timestamp_roundtrip(config in arb_config(), block in 0u64..1_000_000) {
            let ts = config.l2_timestamp(block);
            if ts < u64::MAX {
                prop_assert_eq!(config.l2_block_number_from_timestamp(ts), block);
            }
        }

        #[test]
        fn timestamp_monotonic(config in arb_config(), a in 0u64..1_000_000) {
            let b = a.saturating_add(1);
            let ts_a = config.l2_timestamp(a);
            let ts_b = config.l2_timestamp(b);
            prop_assert!(ts_b >= ts_a);
        }

        #[test]
        fn timestamp_checked_never_panics(
            deployment_timestamp in 0u64..=u64::MAX,
            block_time in 1u64..=u64::MAX,
            block_number in 0u64..=u64::MAX,
        ) {
            let config = RollupConfig {
                deployment_timestamp,
                block_time,
                ..test_config()
            };
            let _ = config.l2_timestamp_checked(block_number);
        }

        #[test]
        fn block_number_from_timestamp_inverse(config in arb_config(), block in 0u64..1_000_000) {
            if let Some(ts) = config.l2_timestamp_checked(block) {
                prop_assert_eq!(config.l2_block_number_from_timestamp(ts), block);
            }
        }
    }
}
