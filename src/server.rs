//! `bore` 服务的服务器端实现。

use std::{io, net::SocketAddr, ops::RangeInclusive, sync::Arc, time::Duration};
use anyhow::Result;
use dashmap::DashMap;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{proxy, ClientMessage, Delimited, ServerMessage, CONTROL_PORT};

/// 服务器的状态结构体。
pub struct Server {

    /// 并发绑定的IP地址。
    bind_ip: String,
    /// 可转发的TCP端口范围。
    port_range: RangeInclusive<u16>,

    /// 用于认证客户端的可选密钥。
    auth: Option<Authenticator>,

    /// ID到入站连接的并发映射。
    conns: Arc<DashMap<Uuid, TcpStream>>,

}

impl Server {
    /// 使用指定的最小端口号创建一个新的服务器。
    pub fn new(bind_ip: Option<&str>,port_range: RangeInclusive<u16>, secret: Option<&str>) -> Self {
        assert!(!port_range.is_empty(), "must provide at least one port");
        info!(bind_ip);
        Server {
            // 如果是空 放入0.0.0.0
            // 如果不是空 放入值
            bind_ip: String::from(bind_ip.unwrap_or("127.0.0.1").to_owned()),
            port_range,
            conns: Arc::new(DashMap::new()),
            auth: secret.map(Authenticator::new),
        }
    }
    
    /// 启动服务器，监听新的连接。
    pub async fn listen(self) -> Result<()> {
        let this = Arc::new(self);
        let addr = SocketAddr::from(([0, 0, 0, 0], CONTROL_PORT));
        let listener = TcpListener::bind(&addr).await?;
        info!(?addr, "server listening");

        loop {
            let (stream, addr) = listener.accept().await?;
            let this = Arc::clone(&this);
            tokio::spawn(
                async move {
                    info!("incoming connection");
                    if let Err(err) = this.handle_connection(stream).await {
                        warn!(%err, "connection exited with error");
                    } else {
                        info!("connection exited");
                    }
                }
                .instrument(info_span!("control", ?addr)),
            );
        }
    }

    async fn create_listener(&self,bind_ip: Option<String>, port: u16) -> Result<TcpListener, &'static str> {
        let try_bind = |bip:Option<String>,port: u16| async move {
            TcpListener::bind((bip.unwrap_or(String::from(self.bind_ip.as_str())).to_owned().as_str(), port))
                .await
                .map_err(|err| match err.kind() {
                    io::ErrorKind::AddrInUse => "port already in use",
                    io::ErrorKind::PermissionDenied => "permission denied",
                    _ => "failed to bind to port",
                })
        };
        if port > 0 {
            // 客户端绑定了特定的IP
            if !self.port_range.contains(&port) {
                return Err("client port number not in allowed range");
            }
            try_bind(bind_ip.clone(),port).await
        } else {
            // 随机绑定150次，即便占用了85%的端口，随机150次也足够保证找到空闲端口的概率大于99.999%
            for _ in 0..150 {
                let port = fastrand::u16(self.port_range.clone());
                match try_bind(bind_ip.clone(),port).await {
                    Ok(listener) => return Ok(listener),
                    Err(_) => continue,
                }
            }
            Err("failed to find an available port")
        }
    }

    async fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        let mut stream = Delimited::new(stream);
        if let Some(auth) = &self.auth {
            if let Err(err) = auth.server_handshake(&mut stream).await {
                warn!(%err, "server handshake failed");
                stream.send(ServerMessage::Error(err.to_string())).await?;
                return Ok(());
            }
        }

        match stream.recv_timeout().await? {
            Some(ClientMessage::Authenticate(_)) => {
                warn!("unexpected authenticate");
                Ok(())
            }
            Some(ClientMessage::Hello(bind_ip,port)) => {
                let listener = match self.create_listener(bind_ip,port).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        stream.send(ServerMessage::Error(err.into())).await?;
                        return Ok(());
                    }
                };
                let port = listener.local_addr()?.port();
                info!(?port, "new client");
                stream.send(ServerMessage::Hello(port)).await?;

                loop {
                    if stream.send(ServerMessage::Heartbeat).await.is_err() {
                        // 假设TCP连接已断开。
                        return Ok(());
                    }
                    const TIMEOUT: Duration = Duration::from_millis(500);
                    if let Ok(result) = timeout(TIMEOUT, listener.accept()).await {
                        let (stream2, addr) = result?;
                        info!(?addr, ?port, "new connection");

                        let id = Uuid::new_v4();
                        let conns = Arc::clone(&self.conns);

                        conns.insert(id, stream2);
                        tokio::spawn(async move {
                            // 移除过期的条目以避免内存泄漏。
                            sleep(Duration::from_secs(10)).await;
                            if conns.remove(&id).is_some() {
                                warn!(%id, "removed stale connection");
                            }
                        });
                        stream.send(ServerMessage::Connection(id)).await?;
                    }
                }
            }
            Some(ClientMessage::Accept(id)) => {
                info!(%id, "forwarding connection");
                match self.conns.remove(&id) {
                    Some((_, mut stream2)) => {
                        let parts = stream.into_parts();
                        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
                        stream2.write_all(&parts.read_buf).await?;
                        proxy(parts.io, stream2).await?
                    }
                    None => warn!(%id, "missing connection"),
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
}
