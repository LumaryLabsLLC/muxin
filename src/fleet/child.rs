//! A command running on a machine, the way Muxin starts it there: `ssh`
//! on most computers, Muxin's own ssh client where no program can be started
//! (an iPad, see `ssh.rs`). Either one is read and written as a child process.

use std::pin::Pin;
use std::process::{ExitStatus, Output, Stdio};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;

use crate::config::MachineSpec;

pub type Input = Pin<Box<dyn AsyncWrite + Send>>;
pub type Reader = Pin<Box<dyn AsyncRead + Send>>;

pub enum Child {
    Process(tokio::process::Child),
    // Started only on an iPad (`ssh` below).
    #[cfg(any(target_os = "ios", all(test, unix)))]
    #[cfg_attr(not(target_os = "ios"), allow(dead_code))]
    Ssh(super::ssh::RemoteChild),
}

impl Child {
    /// Starts a program here, with its stdin, stdout and stderr piped.
    pub fn local(mut cmd: Command) -> std::io::Result<Child> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cmd.spawn().map(Child::Process)
    }

    pub fn take_stdin(&mut self) -> Option<Input> {
        match self {
            Child::Process(c) => c.stdin.take().map(|s| Box::pin(s) as Input),
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.stdin.take().map(|s| Box::pin(s) as Input),
        }
    }

    pub fn take_stdout(&mut self) -> Option<Reader> {
        match self {
            Child::Process(c) => c.stdout.take().map(|s| Box::pin(s) as Reader),
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.stdout.take().map(|s| Box::pin(s) as Reader),
        }
    }

    pub fn take_stderr(&mut self) -> Option<Reader> {
        match self {
            Child::Process(c) => c.stderr.take().map(|s| Box::pin(s) as Reader),
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.stderr.take().map(|s| Box::pin(s) as Reader),
        }
    }

    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Child::Process(c) => c.wait().await,
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.wait().await,
        }
    }

    pub async fn wait_with_output(self) -> std::io::Result<Output> {
        match self {
            Child::Process(c) => c.wait_with_output().await,
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.wait_with_output().await,
        }
    }

    /// Ends it and waits for that.
    pub async fn kill(&mut self) {
        match self {
            Child::Process(c) => {
                let _ = c.kill().await;
            }
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.kill(),
        }
    }

    pub fn start_kill(&mut self) {
        match self {
            Child::Process(c) => {
                let _ = c.start_kill();
            }
            #[cfg(any(target_os = "ios", all(test, unix)))]
            Child::Ssh(c) => c.kill(),
        }
    }
}

/// Starts `command` on a machine, as `ssh <machine> <command>`: with the
/// `ssh` program, or on an iPad with Muxin's own client. Errors say "ssh"
/// and why, in ssh's words where there are some.
pub async fn ssh(spec: &MachineSpec, command: &str) -> Result<Child, String> {
    #[cfg(target_os = "ios")]
    {
        super::ssh::spawn(spec, command).await.map(Child::Ssh)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let mut cmd = Command::new("ssh");
        cmd.args(crate::probe::ssh_args(spec)).arg(command);
        Child::local(cmd).map_err(|e| format!("Couldn't run ssh: {e}"))
    }
}
