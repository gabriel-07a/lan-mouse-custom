//! The workspace test-support crate is wired into the root package and its
//! deterministic fixtures behave as documented.

use lan_mouse_proto::{Position, ProtoEvent};
use lan_mouse_test_support::{
    assertions::assert_close,
    clock::TestClock,
    env::TestEnv,
    wire::{decode, encode, legacy_enter_bytes, legacy_hello_bytes},
};
use std::time::Duration;

#[test]
fn test_env_is_isolated_and_writable() {
    let env = TestEnv::new("root-smoke");
    env.write("config/config.toml", "[clipboard]\nenabled = true\n");
    assert_eq!(
        env.read("config/config.toml"),
        "[clipboard]\nenabled = true\n"
    );
    assert!(env.config_dir().exists());
    assert!(env.runtime_dir().exists());
}

#[test]
fn test_clock_is_deterministic() {
    let clock = TestClock::new();
    let start = clock.now();
    clock.advance(Duration::from_millis(250));
    assert_eq!(clock.now(), start + Duration::from_millis(250));
    assert_eq!(clock.now(), clock.now());
}

#[test]
fn legacy_wire_fixtures_stay_compatible_with_the_production_decoder() {
    let enter = decode(&legacy_enter_bytes(Position::Right)).expect("legacy Enter");
    assert_eq!(
        encode(enter),
        encode(ProtoEvent::Enter {
            pos: Position::Right,
            serial: 0,
        })
    );

    let hello = decode(&legacy_hello_bytes(*b"legacy00")).expect("legacy Hello");
    match hello {
        ProtoEvent::Hello {
            proto_version,
            capabilities,
            ..
        } => {
            assert_eq!(proto_version, 0);
            assert_eq!(capabilities, 0);
        }
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn assertion_helpers_are_usable_from_integration_tests() {
    assert_close(0.0, 0.0, 0.0);
}
