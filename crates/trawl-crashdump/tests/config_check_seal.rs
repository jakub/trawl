// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[test]
#[cfg(target_os = "linux")]
fn config_check_seals_only_the_child_process() {
    const CHILD: &str = "TRAWL_TEST_CONFIG_CHECK_SEAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        trawl_crashdump::seal_for_config_check().unwrap();
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        for name in ["CapEff:", "CapPrm:"] {
            let value = status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap();
            let bits = u64::from_str_radix(value.trim(), 16).unwrap();
            assert_eq!(bits & (1 << 19), 0, "ptrace remains in {name}");
        }
        assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
        return;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config_check_seals_only_the_child_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
