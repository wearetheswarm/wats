//! `--hermetic` / `WATS_HERMETIC`: tests that can never reach the public
//! swarm, whatever config.toml says.

#![cfg(unix)]

use std::time::Duration;

fn wats(data: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env_remove("WATS_INFRA")
        .env_remove("WATS_BOOTSTRAP")
        .env_remove("WATS_PEERS")
        .arg("--dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn hermetic_refuses_remote_bootstraps() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    let router = "9132c3e3e9a3313114adecc9a45fba05721ec2125fcd3f51494a376acdd603d8";
    std::fs::write(
        data.join("config.toml"),
        format!("infra = \"wats\"\nbootstrap = [\"{router}@router.wearetheswarm.ai:7842\"]\n"),
    )
    .unwrap();
    let out = wats(&data, &["id"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--hermetic: bootstrap router.wearetheswarm.ai"),
        "{err}"
    );
    // `wats config` still works, to fix the file.
    assert!(
        wats(&data, &["config", "unset", "bootstrap"])
            .status
            .success()
    );
    std::fs::write(
        data.join("config.toml"),
        format!("bootstrap = [\"{router}@127.0.0.1:7842\"]\n"),
    )
    .unwrap();
    assert!(wats(&data, &["id"]).status.success());
}

#[test]
fn hermetic_daemon_is_loopback_only() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    // Even if the file asks for the public infrastructure.
    std::fs::write(data.join("config.toml"), "infra = \"wats\"\n").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(&data)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("infra none") && err.contains("LOOPBACK ONLY"),
        "{err}"
    );
    assert!(
        !err.contains("bootstrap peer"),
        "no default bootstrap: {err}"
    );
}
