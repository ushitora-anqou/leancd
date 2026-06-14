//! SSH keypair generation for the SSH-transport scenario.

use std::fs;
use std::process::Command;

/// Generate an ed25519 keypair in a temp dir and return `(public, private)`.
pub fn generate_keypair() -> (String, String) {
    let dir = std::env::temp_dir().join(format!("leancd-sshkey-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let key = dir.join("id_ed25519");
    let status = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            key.to_str().unwrap(),
            "-C",
            "leancd-e2e",
        ])
        .status()
        .expect("ssh-keygen spawn");
    assert!(status.success(), "ssh-keygen failed");
    let pub_key = fs::read_to_string(format!("{}.pub", key.display())).unwrap();
    let priv_key = fs::read_to_string(&key).unwrap();
    let _ = fs::remove_dir_all(&dir);
    (pub_key, priv_key)
}
