use anyhow::Result;
use bore_cli::{client::Client, server::Server};
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 启动本地代理到远程服务器。
    Local {
        /// 要暴露的本地端口。
        #[clap(env = "BORE_LOCAL_PORT")]
        local_port: u16,

        /// 最大本地端口（用于端口范围转发）。
        #[clap(short, long, value_name = "MAX_PORT")]
        max_port: Option<u16>,

        /// 要暴露的本地主机。
        #[clap(short, long, value_name = "HOST", default_value = "localhost")]
        local_host: String,

        /// 远程服务器的地址，用于暴露本地端口。
        #[clap(short, long, env = "BORE_SERVER")]
        to: String,

        /// 可选的远程服务器端口。
        #[clap(short, long, default_value_t = 0)]
        port: u16,

        /// 可选的认证密钥。
        #[clap(short, long, env = "BORE_SECRET",default_value="hpc.pub", hide_env_values = true)]
        secret: Option<String>,

        /// 可选的绑定地址（IPv4）。
        #[clap(short, long, env = "BORE_BIND", default_value="127.0.0.1")]
        bind_ip: Option<String>,

    },

    /// 运行远程代理服务器。
    Server {
        /// 最小允许的TCP端口号。
        #[clap(long, default_value_t = 1024, env = "BORE_MIN_PORT")]
        min_port: u16,

        /// 最大允许的TCP端口号。
        #[clap(long, default_value_t = 65535, env = "BORE_MAX_PORT")]
        max_port: u16,

        /// 可选的认证密钥。
        #[clap(short, long, env = "BORE_SECRET", default_value="hpc.pub", hide_env_values = true)]
        secret: Option<String>,


        /// 可选的绑定地址（IPv4）。
        #[clap(short, long, env = "BORE_BIND", default_value="127.0.0.1")]
        bind_ip: Option<String>,
    },
}

/// 解析 IP 范围字符串，如 "127.0.0.1-10"
fn parse_ip_range(ip_range: &str) -> Result<Vec<String>> {
    if let Some(dash_pos) = ip_range.find('-') {
        let base_ip = &ip_range[..dash_pos];
        let end_num_str = &ip_range[dash_pos + 1..];
        
        // 解析基础 IP 地址
        let ip_parts: Vec<&str> = base_ip.split('.').collect();
        if ip_parts.len() != 4 {
            return Err(anyhow::anyhow!("Invalid IP format: {}", base_ip));
        }
        
        // 解析结束数字
        let end_num: u8 = end_num_str.parse()
            .map_err(|_| anyhow::anyhow!("Invalid end number: {}", end_num_str))?;
        
        // 解析基础 IP 的最后一部分
        let base_last: u8 = ip_parts[3].parse()
            .map_err(|_| anyhow::anyhow!("Invalid IP last octet: {}", ip_parts[3]))?;
        
        if end_num < base_last {
            return Err(anyhow::anyhow!("End number must be greater than or equal to base number"));
        }
        
        // 生成 IP 地址列表
        let mut ips = Vec::new();
        for i in base_last..=end_num {
            let ip = format!("{}.{}.{}.{}", ip_parts[0], ip_parts[1], ip_parts[2], i);
            ips.push(ip);
        }
        
        Ok(ips)
    } else {
        // 单个 IP 地址
        Ok(vec![ip_range.to_string()])
    }
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
    match command {
        Command::Local {
            local_host,
            local_port,
            max_port,
            to,
            port,
            secret,
            bind_ip
        } => {
            // 检查是否包含 IP 范围格式
            let is_ip_range = local_host.contains('-');
            
            // 验证互斥性
            if is_ip_range && max_port.is_some() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "Cannot use both IP range (-l with range) and port range (-m) at the same time")
                    .exit();
            }
            
            if is_ip_range {
                // IP 范围转发
                let ip_list = parse_ip_range(&local_host)?;
                let mut clients = Vec::with_capacity(ip_list.len());
                
                for (i, ip) in ip_list.iter().enumerate() {
                    let current_remote_port = if port > 0 { port + i as u16 } else { 0 };
                    
                    let client = Client::new(
                        ip, 
                        local_port, 
                        &to, 
                        current_remote_port, 
                        secret.as_deref(),
                        bind_ip.as_deref()
                    ).await?;
                    clients.push(client);
                }
                
                // 启动所有客户端
                let mut handles = Vec::new();
                for client in clients {
                    let handle = tokio::spawn(async move {
                        client.listen().await
                    });
                    handles.push(handle);
                }
                
                // 等待所有客户端完成
                for handle in handles {
                    handle.await??;
                }
            } else if let Some(max_port) = max_port {
                // 端口范围转发
                if max_port < local_port {
                    Args::command()
                        .error(ErrorKind::InvalidValue, "max port must be greater than or equal to local port")
                        .exit();
                }
                let port_count = (max_port - local_port + 1) as usize;
                let mut clients = Vec::with_capacity(port_count);
                
                for i in 0..port_count {
                    let current_local_port = local_port + i as u16;
                    let current_remote_port = if port > 0 { port + i as u16 } else { 0 };
                    
                    let client = Client::new(
                        &local_host, 
                        current_local_port, 
                        &to, 
                        current_remote_port, 
                        secret.as_deref(),
                        bind_ip.as_deref()
                    ).await?;
                    clients.push(client);
                }
                
                // 启动所有客户端
                let mut handles = Vec::new();
                for client in clients {
                    let handle = tokio::spawn(async move {
                        client.listen().await
                    });
                    handles.push(handle);
                }
                
                // 等待所有客户端完成
                for handle in handles {
                    handle.await??;
                }
            } else {
                // 单个端口转发（原有逻辑）
                let client = Client::new(&local_host, local_port, &to, port, secret.as_deref(),bind_ip.as_deref()).await?;
                client.listen().await?;
            }
        }
        Command::Server {
            min_port,
            max_port,
            secret,
            bind_ip
        } => {
            let port_range = min_port..=max_port;
            if port_range.is_empty() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "port range is empty")
                    .exit();
            }
            Server::new(bind_ip.as_deref(),port_range, secret.as_deref()).listen().await?;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    run(Args::parse().command)
}
