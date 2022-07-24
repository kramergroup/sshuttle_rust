use std::io::IoSliceMut;
use std::net::IpAddr;
use std::os::unix::prelude::AsRawFd;
use std::sync::Arc;
use std::time::Duration;
use std::{error::Error, fmt::Display, net::SocketAddr};

use fast_socks5::client::Socks5Stream;

use nix::cmsg_space;
use nix::errno::Errno;
use nix::sys::socket::sockopt::IpTransparent;
use nix::sys::socket::{
    bind, recvmsg, setsockopt, socket, AddressFamily, ControlMessageOwned, MsgFlags, RecvMsg,
    SockFlag, SockType, SockaddrIn,
};
use tokio::io::{copy_bidirectional, Interest};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio::time::sleep;
use tokio::{process::Command, spawn, task::JoinHandle};

use crate::command::CommandError;
use crate::firewall::{
    Firewall, FirewallConfig, FirewallError, FirewallListenerConfig, FirewallSubnetConfig,
};
use crate::network::{ListenerAddr, Subnets};
use crate::options::FirewallType;

pub struct Config {
    pub includes: Subnets,
    pub excludes: Subnets,
    pub remote: Option<String>,
    pub listen: Vec<ListenerAddr>,
    pub socks_addr: SocketAddr,
    pub firewall: FirewallType,
}

#[derive(Debug)]
pub struct ClientError {
    message: String,
}

impl Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl From<FirewallError> for ClientError {
    fn from(err: FirewallError) -> Self {
        ClientError {
            message: format!("FirewallError: {err}"),
        }
    }
}

impl From<JoinError> for ClientError {
    fn from(err: JoinError) -> Self {
        ClientError {
            message: format!("JoinError: {err}"),
        }
    }
}

impl From<CommandError> for ClientError {
    fn from(err: CommandError) -> Self {
        ClientError {
            message: format!("CommandError: {err}"),
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(err: std::io::Error) -> Self {
        ClientError {
            message: format!("std::io::Error: {err}"),
        }
    }
}

impl From<mpsc::error::SendError<Message>> for ClientError {
    fn from(err: mpsc::error::SendError<Message>) -> Self {
        ClientError {
            message: format!("mpsc::error::SendError: {err}"),
        }
    }
}

impl Error for ClientError {}

pub async fn main(config: &Config) -> Result<(), ClientError> {
    let (control_tx, control_rx) = mpsc::channel(1);

    let tx_clone = control_tx.clone();
    ctrlc::set_handler(move || {
        tx_clone
            .blocking_send(Message::Shutdown)
            .expect("Could not send signal on channel.")
    })
    .expect("Error setting Ctrl-C handler");

    let firewall_config = get_firewall_config(config);
    let firewall = get_firewall(config)?;
    let setup_commands = firewall.setup_firewall(&firewall_config)?;
    let shutdown_commands = firewall.restore_firewall(&firewall_config)?;

    log::info!("Setting up firewall {:#?}", setup_commands);
    setup_commands.run_all().await?;

    log::debug!("run_everything");
    let client_result = run_everything(config, firewall, control_tx, control_rx).await;
    if let Err(err) = &client_result {
        log::error!("run_everything error: {err}");
    } else {
        log::debug!("run_everything exited normally");
    }

    log::info!("Restoring firewall{:#?}", shutdown_commands);
    let shutdown_result = shutdown_commands.run_all().await;
    if let Err(err) = &shutdown_result {
        log::error!("Error restoring firewall: {err}");
    } else {
        log::debug!("Restored firewall");
    }

    client_result?;
    shutdown_result?;
    Ok(())
}

async fn run_everything(
    config: &Config,
    firewall: Box<dyn Firewall + Send + Sync>,
    control_tx: mpsc::Sender<Message>,
    mut control_rx: mpsc::Receiver<Message>,
) -> Result<(), ClientError> {
    let client = run_client(config, firewall);

    if let Some(remote) = &config.remote {
        // ssh shutdown sequence with ssh:
        // ctrlc handler sends signal to control_tx.
        // ssh handler receives event from control_rx.
        // ssh handler kills ssh.
        // ssh_handle completes, and the select finishes.
        // we return.
        let c = run_ssh(config, remote.to_string(), control_rx).await?;
        let ssh_handle = c.handle;

        tokio::pin!(ssh_handle);
        tokio::pin!(client);

        select! {
            res = &mut ssh_handle => {
                log::info!("ssh_handle finished");
                res??;
            },
            res = &mut client => {
                log::info!("client finished");
                res?;
            },
            else => {
                log::info!("everything finished");
            }
        }

        // We don't care if the message fails, probably because ssh already exited.
        _ = control_tx.send(Message::Shutdown).await;
    } else {
        // ssh shutdown sequence without ssh:
        // ctrlc handler sends signal to control_tx.
        // the select finishes.
        // we return.
        select! {
            res = client => {
                log::info!("client finished");
                res?;
            },
            Some(_) = control_rx.recv() => {
                log::info!("control_rx shutdown requested");
            }
        }
    }

    Ok(())
}

// async fn read_tcpstream(
//     stream: &mut TcpStream,
//     buf: &mut [u8],
//     shutdown: bool,
// ) -> Option<Result<usize, std::io::Error>> {
//     if shutdown {
//         None
//     } else {
//         Some(stream.read(buf).await)
//     }
// }

// async fn read_socksstream(
//     stream: &mut Socks5Stream<TcpStream>,
//     buf: &mut [u8],
//     shutdown: bool,
// ) -> Option<Result<usize, std::io::Error>> {
//     if shutdown {
//         None
//     } else {
//         Some(stream.read(buf).await)
//     }
// }

// async fn write(stream: &mut Option<TcpStream>, buf: &[u8]) -> Option<Result<(), std::io::Error>> {
//     if let Some(s) = stream {
//         Some(s.write_all(buf).await)
//     } else {
//         None
//     }
// }

fn get_firewall(config: &Config) -> Result<Box<dyn Firewall + Send + Sync>, ClientError> {
    let firewall: Box<dyn Firewall + Send + Sync> = match config.firewall {
        FirewallType::Nat => Box::new(crate::firewall::nat::NatFirewall::new()),
        FirewallType::TProxy => Box::new(crate::firewall::tproxy::TProxyFirewall::new()),
    };
    Ok(firewall)
}

fn get_firewall_config(config: &Config) -> FirewallConfig {
    let familys = config
        .listen
        .iter()
        .map(|addr| match addr.ip() {
            IpAddr::V4(_) => FirewallListenerConfig::Ipv4(FirewallSubnetConfig {
                enable: true,
                listener: addr.clone(),
                includes: config.includes.ipv4(),
                excludes: config.excludes.ipv4(),
            }),
            IpAddr::V6(_) => FirewallListenerConfig::Ipv6(FirewallSubnetConfig {
                enable: true,
                listener: addr.clone(),
                includes: config.includes.ipv6(),
                excludes: config.excludes.ipv6(),
            }),
        })
        .collect();
    FirewallConfig {
        filter_from_user: None,
        listeners: familys,
    }
}

#[derive(Debug, Clone)]
enum Message {
    Shutdown,
}

struct Task {
    // tx: mpsc::Sender<Message>,
    handle: JoinHandle<Result<(), std::io::Error>>,
}

async fn run_ssh(
    config: &Config,
    remote: String,
    mut rx: mpsc::Receiver<Message>,
) -> Result<Task, ClientError> {
    let socks = config.socks_addr;

    let handle: JoinHandle<Result<(), std::io::Error>> = spawn(async move {
        let args = vec![
            "-D".to_string(),
            socks.to_string(),
            "-N".to_string(),
            remote,
        ];

        let mut child = Command::new("ssh").args(args).spawn()?;

        tokio::select! {
            msg = rx.recv() => {
                log::info!("ssh shutdown requested, killing child ssh: {msg:?}");
                child.kill().await?;
                Ok(())
            }
            status = child.wait() => {
                match status {
                    Ok(rc) => {
                        if rc.success() {
                            log::error!("ssh exited with rc: {rc}");
                            Ok(())
                        } else {
                            log::info!("ssh exited with rc: {rc}");
                            Err(std::io::Error::new(std::io::ErrorKind::Other, "ssh failed"))
                        }
                    }
                    Err(err) => {
                        log::error!("ssh wait failed: {err}");
                        Err(err)
                    }
                }
            }
        }
    });

    Ok(Task { handle })
}

async fn run_client(
    config: &Config,
    firewall: Box<dyn Firewall + Send + Sync>,
) -> Result<Task, ClientError> {
    let socks_addr = config.socks_addr;
    let listen = config.listen.clone();

    let firewall: Arc<dyn Firewall + Send + Sync> = Arc::from(firewall);
    for l_addr in listen {
        println!("----> {}", l_addr);
        match l_addr.protocol {
            crate::network::Protocol::Tcp => listen_tcp(&firewall, l_addr, socks_addr).await?,
            crate::network::Protocol::Udp => listen_udp(&firewall, l_addr, socks_addr).await?,
        }
    }

    loop {
        sleep(Duration::from_secs(60)).await;
    }
}

async fn listen_tcp(
    firewall: &Arc<dyn Firewall + Send + Sync>,
    l_addr: ListenerAddr,
    socks_addr: SocketAddr,
) -> Result<(), ClientError> {
    let firewall = Arc::clone(firewall);
    let listener = TcpListener::bind(l_addr.addr).await?;
    firewall.setup_tcp_listener(&listener)?;

    let _handle = tokio::spawn(async move {
        loop {
            let firewall = Arc::clone(&firewall);
            let (socket, _) = listener.accept().await.unwrap();
            let l_addr = l_addr.clone();
            tokio::spawn(async move {
                handle_tcp_client(socket, &l_addr, socks_addr, firewall).await;
            });
        }
    });
    Ok(())
}

async fn listen_udp(
    firewall: &Arc<dyn Firewall + Send + Sync>,
    l_addr: ListenerAddr,
    _socks_addr: SocketAddr,
) -> Result<(), ClientError> {
    let _firewall = Arc::clone(firewall);
    let firewall = Arc::clone(firewall);
    let local = UdpSocket::bind(l_addr.addr).await?;
    // local.set_nonblocking(false)?;
    firewall.setup_udp_socket(&local)?;

    let _handle = tokio::spawn(async move {
        loop {
            // let firewall = Arc::clone(&firewall);
            // let mut buf = [0u8; 65535];

            // let (len, addr) = local.recv_from(&mut buf).await.unwrap();
            // let l_addr = l_addr.clone();

            let mut buf = vec![0u8; 1024];
            let mut iov = [IoSliceMut::new(&mut buf)];

            let mut cmsg = cmsg_space!(libc::in6_addr);

            log::debug!("udp readable?");
            local.readable().await.unwrap();

            log::debug!("recvmesg");
            let msg: Result<RecvMsg<()>, _> = recvmsg(
                local.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg),
                MsgFlags::empty(),
            );
            log::debug!("recvmsg: {msg:?}", msg = msg);

            let msg = match msg {
                Ok(msg) => msg,
                Err(Errno::EAGAIN) => {
                    continue;
                }
                Err(err) => {
                    log::error!("recvmsg failed: {err}");
                    continue;
                }
            };

            for cmsg in msg.cmsgs() {
                match cmsg {
                    ControlMessageOwned::Ipv4RecvOrigDstAddr(addr) => {
                        println!("IPv4 {addr:?}");
                    }
                    ControlMessageOwned::Ipv6RecvOrigDstAddr(addr) => {
                        println!("IPv6 {addr:?}");
                    }
                    _ => panic!("unexpected additional control msg"),
                }
            }
        }
    });
    Ok(())

    // use nix::sys::socket::sockopt::Ipv4RecvOrigDstAddr;
    // let s: SockaddrIn = "127.0.0.1:12300".parse().unwrap();
    // let receive = socket(
    //     AddressFamily::Inet,
    //     SockType::Datagram,
    //     SockFlag::empty(),
    //     None,
    // )
    // .expect("receive socket failed");
    // setsockopt(receive, IpTransparent, &true).unwrap();
    // bind(receive, &s).expect("bind failed");
    // // let sa: SockaddrIn = getsockname(receive).expect("getsockname failed");
    // setsockopt(receive, Ipv4RecvOrigDstAddr, &true).expect("setsockopt IP_RECVDSTADDR failed");
    // // let value = 1u8;
    // // let value_ptr: *const libc::c_void = &value as *const u8 as *const libc::c_void;
    // // unsafe {
    // //     libc::setsockopt(
    // //         receive,
    // //         libc::IPPROTO_IP,
    // //         libc::IP_RECVORIGDSTADDR,
    // //         value_ptr,
    // //         std::mem::size_of::<u8>() as u32,
    // //     )
    // // };

    // tokio::spawn(async move {
    //     loop {
    //         // let iov = IoSliceMut::new(&mut buf);
    //         // let mut cmsg = vec![0u8; 48];
    //         let l_addr = l_addr.clone();
    //         println!("{l_addr} UDP waiting");
    //         let _: Result<_, ClientError> = tokio::task::spawn_blocking(move || {
    //             let mut buf = vec![0u8; 1024];
    //             let mut iov = [IoSliceMut::new(&mut buf)];

    //             let mut cmsg = cmsg_space!(libc::in_addr);
    //             let msg: RecvMsg<()> =
    //                 recvmsg(receive, &mut iov, Some(&mut cmsg), MsgFlags::empty()).unwrap();
    //             for cmsg in msg.cmsgs() {
    //                 match cmsg {
    //                     ControlMessageOwned::Ipv4RecvOrigDstAddr(addr) => {
    //                         println!("{addr:?}");
    //                     }
    //                     _ => panic!("unexpected additional control msg"),
    //                 }
    //             }
    //             println!("{l_addr} UDP {msg:?}");
    //             Ok(())
    //         })
    //         .await
    //         .unwrap();
    //     }
    // });
    // // })
    // // .await
    // // .unwrap();
    // Ok(())
}

async fn handle_tcp_client(
    socket: TcpStream,
    l_addr: &ListenerAddr,
    socks_addr: SocketAddr,
    firewall: Arc<dyn Firewall + Send + Sync>,
) {
    let mut local = socket;
    let local_addr = local.peer_addr().unwrap();
    log::debug!("new connection from: {}", local_addr);

    let remote_addr = firewall.get_dst_addr(&local).unwrap();
    log::info!("{l_addr} got connection from {local_addr} to {remote_addr}");

    let (addr_str, port) = {
        let addr = remote_addr.ip().to_string();
        let port = remote_addr.port();
        (addr, port)
    };

    let mut remote_config = fast_socks5::client::Config::default();
    remote_config.set_skip_auth(false);
    let mut remote = Socks5Stream::connect(socks_addr, addr_str, port, remote_config)
        .await
        .unwrap();

    let result = copy_bidirectional(&mut local, &mut remote).await;
    // let result = my_bidirectional_copy(&mut local, &mut remote).await;

    log::debug!("copy_bidirectional result: {:?}", result);
}

// async fn my_bidirectional_copy(
//     local: &mut TcpStream,
//     remote: &mut Socks5Stream<TcpStream>,
// ) -> Result<(), ClientError> {
//     let mut local_buf = [0; 1024];
//     let mut remote_buf = [0; 1024];
//     let remote_shutdown: bool = false;
//     let mut local_shutdown: bool = false;

//     println!("start loop");
//     loop {
//         println!("start select");
//         select! {
//             Some(res) = read_tcpstream(local, &mut local_buf, local_shutdown) => {
//                 println!("local read");
//                 match res {
//                     Ok(0) => {
//                         println!("local shutdown request");
//                         remote.shutdown().await.unwrap();
//                         local_shutdown = true;
//                     }
//                     Ok(n) => {
//                         println!("local read -> remote write: {}", n);
//                         remote.write_all(&local_buf[..n]).await.unwrap();
//                     }
//                     Err(err) => {
//                         println!("local read failed: {}", err);
//                         remote.shutdown().await.unwrap();
//                         break;
//                     }
//                 }
//             }
//             Some(res) = read_socksstream(remote, &mut remote_buf, remote_shutdown) => {
//                 println!("remote read {:?}", res);
//                 match res {
//                     Ok(0) => {
//                         println!("remote shutdown request");
//                         let _ = local.shutdown().await.map_err(|err| {log::warn!("local shutdown failed {err}"); err});                        // remote_shutdown = true;
//                         break;
//                     }
//                     Ok(n) => {
//                         println!("remote read -> local write: {} {}", n, remote_shutdown);
//                         println!("{:?}", &remote_buf[..n]);
//                         local.write_all(&remote_buf[..n]).await.unwrap();
//                     }
//                     Err(err) => {
//                         println!("remote read failed: {}", err);
//                         local.shutdown().await.unwrap();
//                         break;
//                     }
//                 }
//             }
//             else => {
//                 print!("else Shutdown");
//                 break;
//             }
//         }
//         println!("end select");
//     }
//     println!("end loop");

//     Ok(())
// }
