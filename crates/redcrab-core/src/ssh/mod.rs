//! SSH tunnels to servers that are only reachable through a bastion host.
//! See [`crate::SshConfig`] for the settings and [`KnownHosts`] for how
//! host keys are checked.

mod known_hosts;
mod tunnel;

pub(crate) use self::tunnel::Tunnel;
pub use self::{
    known_hosts::{HostKeyError, HostKeyProblem, KnownHosts},
    tunnel::TunnelOptions,
};
