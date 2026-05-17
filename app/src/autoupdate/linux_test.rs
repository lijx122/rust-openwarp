use super::*;

#[test]
fn test_package_name() {
    assert_eq!(package_name(Channel::Dev), "warp-terminal-dev");
    assert_eq!(package_name(Channel::Stable), "warp-terminal");
}
