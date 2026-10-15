//! Phase 0 spike, as a user would run it: two `wats node` processes with
//! separate data directories exchange a signed event and a blob.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

struct Proc {
    child: Child,
    lines: mpsc::Receiver<Value>,
}

impl Proc {
    fn spawn(args: &[&str]) -> Proc {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wats"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn wats");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        Proc { child, lines }
    }

    fn expect(&self, kind: &str) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("timed out waiting for {kind}"));
            if line["type"] == kind {
                return line;
            }
            assert_ne!(line["type"], "error", "{line}");
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn two_processes_exchange_event_and_blob() {
    let dir = tempfile::tempdir().unwrap();
    let dir_a = dir.path().join("a");
    let dir_b = dir.path().join("b");
    let file = dir.path().join("notes.md");
    std::fs::write(&file, "# Findings\nThe swarm works.\n").unwrap();

    let a = Proc::spawn(&[
        "--loopback",
        "--pow-bits=8",
        "--min-pow-bits=8",
        "--dir",
        dir_a.to_str().unwrap(),
        "node",
    ]);
    let ready = a.expect("ready");
    let ticket = ready["ticket"].as_str().unwrap().to_string();

    let b = Proc::spawn(&[
        "--loopback",
        "--pow-bits=8",
        "--min-pow-bits=8",
        "--dir",
        dir_b.to_str().unwrap(),
        "--peer",
        &ticket,
        "node",
        "--post",
        "Findings attached.",
        "--kind",
        "progress",
        "--tag",
        "project:spike",
        "--blob",
        file.to_str().unwrap(),
    ]);
    let b_ready = b.expect("ready");
    let published = b.expect("published");

    let event = a.expect("event");
    assert_eq!(event["event"]["id"], published["id"]);
    assert_eq!(event["event"]["author"], b_ready["author"]);
    assert_eq!(event["event"]["kind"], "progress");
    let blob = a.expect("blob");
    assert_eq!(blob["name"], "notes.md");
    assert_eq!(blob["hash"], event["event"]["blobs"][0]["hash"]);
}
