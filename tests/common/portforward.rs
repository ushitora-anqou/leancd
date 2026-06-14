//! `kubectl port-forward` lifecycle with readiness polling.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A running `kubectl port-forward` that forwards `127.0.0.1:<local_port>` to a
/// remote port inside the cluster. Killed on drop.
pub struct PortForward {
    child: Child,
    pub local_port: u16,
}

impl PortForward {
    /// Forward `127.0.0.1:<os-assigned port>` to `<namespace>/<target>:<remote>`.
    pub fn new(namespace: &str, target: &str, remote_port: u16) -> PortForward {
        let local_port = free_port();
        let child = Command::new("kubectl")
            .args([
                "port-forward",
                "-n",
                namespace,
                target,
                &format!("{local_port}:{remote_port}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("kubectl port-forward spawn: {e}"));
        let pf = PortForward { child, local_port };
        pf.wait_until_ready();
        pf
    }

    fn wait_until_ready(&self) {
        let addr = SocketAddr::new("127.0.0.1".parse().unwrap(), self.local_port);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "port-forward to 127.0.0.1:{} not ready in 30s",
            self.local_port
        );
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pick a free TCP port by binding to :0 and reading the assigned port. There
/// is an inherent TOCTOU window between this and the port-forward binding, but
/// on a quiet loopback it is reliable in practice.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind :0")
        .local_addr()
        .unwrap()
        .port()
}
