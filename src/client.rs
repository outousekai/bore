//! `bore` 服务的客户端实现。

use std::process::exit;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};
use tracing::{error, info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    proxy, ClientMessage, Delimited, ServerMessage, CONTROL_PORT, NETWORK_TIMEOUT,
};

/// 客户端的状态结构体。
pub struct Client {
    /// 到服务器的控制连接。
    conn: Option<Delimited<TcpStream>>,
    
    /// 服务器的目标地址。
    to: String,

    // 被转发的本地主机。
    local_host: String,

    /// 被转发的本地端口。
    local_port: u16,

    /// 在远程公开可用的端口。
    remote_port: u16,

    /// 用于认证客户端的可选密钥。
    auth: Option<Authenticator>,
    
    // // 提供给服务端,进行绑定的IP地址
    // #[allow(dead_code)]
    // server_bind_ip: String,
}

impl Client {
    /// 创建一个新的客户端。
    pub async fn new(
        local_host: &str,
        local_port: u16,
        to: &str,
        port: u16,
        secret: Option<&str>,
        bind_ip: Option<&str>
    ) -> Result<Self> {
        let mut stream = Delimited::new(connect_with_timeout(to, CONTROL_PORT).await?);
        let auth = secret.map(Authenticator::new);
        if let Some(auth) = &auth {
            auth.client_handshake(&mut stream).await?;
        }
        stream.send(ClientMessage::Hello(bind_ip.map(|s| s.to_string()),port)).await?;
        let remote_port = match stream.recv_timeout().await? {
            Some(ServerMessage::Hello(remote_port)) => remote_port,
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected initial non-hello message"),
            None => bail!("unexpected EOF"),
        };
        #[cfg(windows)]
        info!("{local_host}:{local_port} 实际连接地址 {to}:{remote_port} 允许访问范围 {bind_ip:?}");
        #[cfg(not(windows))]
        info!("{local_host}:{local_port} listening at {to}:{remote_port} bind_ip is {bind_ip:?}");

        Ok(Client {
            conn: Some(stream),
            to: to.to_string(),
            local_host: local_host.to_string(),
            local_port,
            remote_port,
            auth,
            // server_bind_ip: bind_ip.unwrap_or("127.0.0.1").to_string(),
        })
    }

    /// 返回远程公开可用的端口。
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// 启动客户端，监听新的连接。
    pub async fn listen(mut self) -> Result<()> {
        let mut conn = self.conn.take().unwrap();
        let this = Arc::new(self);
        loop {
            match conn.recv_timeout().await? {
                Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                Some(ServerMessage::Heartbeat) => (),
                Some(ServerMessage::Connection(id)) => {
                    let this = Arc::clone(&this);
                    tokio::spawn(
                        async move {
                            info!("new connection");
                            match this.handle_connection(id).await {
                                Ok(_) => info!("connection exited"),
                                Err(err) => warn!(%err, "connection exited with error"),
                            }
                        }
                        .instrument(info_span!("proxy", %id)),
                    );
                }
                Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                None => {
                    warn!("server no have return msg,close this client now");
                    exit(1);
                    // return Ok(())
                },
            }
        }
    }

    /// 处理单个代理连接。
    async fn handle_connection(&self, id: Uuid) -> Result<()> {
        let mut remote_conn =
            Delimited::new(connect_with_timeout(&self.to[..], CONTROL_PORT).await?);
        if let Some(auth) = &self.auth {
            auth.client_handshake(&mut remote_conn).await?;
        }
        remote_conn.send(ClientMessage::Accept(id)).await?;
        let mut local_conn = connect_with_timeout(&self.local_host, self.local_port).await?;
        let parts = remote_conn.into_parts();
        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
        local_conn.write_all(&parts.read_buf).await?; // 大多数情况下，这里是空的
        proxy(local_conn, parts.io).await?;
        Ok(())
    }
}

/// 尝试在指定超时时间内连接到目标地址和端口。
async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))
}
