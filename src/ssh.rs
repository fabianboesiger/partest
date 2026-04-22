use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::client;
use russh::ChannelMsg;
use russh_keys::key::PrivateKeyWithHashAlg;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use tracing::debug;

/// Minimal SSH client handler — accepts all host keys (local network dev tool).
struct ClientHandler;

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct Session {
    handle: client::Handle<ClientHandler>,
}

impl Session {
    /// Connect to a peer via SSH.
    pub async fn connect(
        ip: IpAddr,
        port: u16,
        user: &str,
        key_path: &Path,
    ) -> Result<Self> {
        let key_data = tokio::fs::read_to_string(key_path)
            .await
            .with_context(|| format!("failed to read SSH key: {}", key_path.display()))?;

        let key_pair = russh_keys::decode_secret_key(&key_data, None)
            .context("failed to decode SSH private key")?;

        let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key_pair), None)
            .context("failed to create key with hash alg")?;

        let config = Arc::new(client::Config::default());
        let addr = (ip, port);
        let mut handle = client::connect(config, addr, ClientHandler)
            .await
            .with_context(|| format!("SSH connect failed to {ip}:{port}"))?;

        let auth_ok = handle
            .authenticate_publickey(user, key_with_hash)
            .await
            .context("SSH authentication failed")?;

        if !auth_ok {
            anyhow::bail!("SSH authentication rejected by {ip}:{port}");
        }

        debug!("SSH connected to {ip}:{port} as {user}");
        Ok(Session { handle })
    }

    /// Upload a file to the remote host.
    /// Creates parent directories as needed.
    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let data = tokio::fs::read(local_path)
            .await
            .with_context(|| format!("failed to read {}", local_path.display()))?;

        // Ensure remote directory exists
        let remote_dir = remote_path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        self.exec_ignore(&format!("mkdir -p {remote_dir}")).await?;

        // Use cat to write the file
        let mut channel = self.handle.channel_open_session().await?;
        channel
            .exec(true, format!("cat > {remote_path}"))
            .await?;
        channel.data(&data[..]).await?;
        channel.eof().await?;

        // Wait for channel close
        while let Some(msg) = channel.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = msg {
                if exit_status != 0 {
                    anyhow::bail!("remote cat exited with status {exit_status}");
                }
            }
        }

        debug!("uploaded {} -> remote:{remote_path}", local_path.display());
        Ok(())
    }

    /// Execute a command and stream stdout/stderr lines via a callback.
    /// Returns the exit status.
    pub async fn exec_stream<F>(&self, command: &str, mut on_line: F) -> Result<u32>
    where
        F: FnMut(&str),
    {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut buf = String::new();
        let mut exit_code: u32 = 0;

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => {
                    buf.push_str(&String::from_utf8_lossy(data));
                    while let Some(pos) = buf.find('\n') {
                        let line: String = buf.drain(..=pos).collect();
                        on_line(line.trim_end_matches('\n'));
                    }
                }
                ChannelMsg::ExtendedData { ref data, .. } => {
                    buf.push_str(&String::from_utf8_lossy(data));
                    while let Some(pos) = buf.find('\n') {
                        let line: String = buf.drain(..=pos).collect();
                        on_line(line.trim_end_matches('\n'));
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = exit_status;
                }
                _ => {}
            }
        }

        // Flush remaining buffer
        if !buf.is_empty() {
            on_line(&buf);
        }

        Ok(exit_code)
    }

    /// Execute a simple command and discard output.
    pub async fn exec_ignore(&self, command: &str) -> Result<()> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        while let Some(msg) = channel.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = msg {
                if exit_status != 0 {
                    anyhow::bail!("command `{command}` exited with status {exit_status}");
                }
            }
        }
        Ok(())
    }
}
