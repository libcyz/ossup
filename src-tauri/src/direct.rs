//! 绕过代理 / TUN 直连 OSS。
//!
//! Clash、mihomo 这类 TUN 模式代理会接管整张路由表，HTTP_PROXY 之类的环境变量
//! 管不到它；全局模式下连「aliyuncs.com 直连」的规则都会失效，OSS 流量被绕去
//! 境外节点，单连接只剩几 MB/s。
//!
//! TUN 的 auto-route 只接管「没绑源地址」的本机流量（`from 0.0.0.0 iif lo`）。
//! 所以只要做到两件事就能出去：
//! 1. 连接的源地址绑到物理网卡的 IP —— 路由查表落回 main 表，走真网关；
//! 2. 域名不走系统 DNS —— 系统 DNS 被劫持成 fake-ip（198.18.x.x），
//!    绑了源地址再去连 fake-ip 是连不通的。DNS 查询同样从网卡 IP 发出去。
//!
//! ossutil 自己有 `--bind-address`，但解析域名照样走系统 DNS，拿到的还是
//! fake-ip。所以这里在 127.0.0.1 上起一个只认 CONNECT 的小隧道，ossutil 用
//! `--proxy` 指过来，TLS 端到端不受影响。

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};

/// 公共 DNS。OSS 是阿里云的域名，AliDNS 排第一。
const RESOLVERS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(223, 5, 5, 5),
    Ipv4Addr::new(119, 29, 29, 29),
    Ipv4Addr::new(114, 114, 114, 114),
];
const DNS_TTL: Duration = Duration::from_secs(60);
/// 换 Wi-Fi、插网线之后网卡 IP 会变，别缓存太久
const SOURCE_TTL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

struct Tunnel {
    port: u16,
    token: String,
}

static TUNNEL: OnceLock<Tunnel> = OnceLock::new();

/// 应用启动时调一次。端口同步绑定好，之后 `proxy_url` 立刻可用。
pub fn start() -> io::Result<()> {
    let std_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    let token = random_token();
    let expected = format!("Basic {}", B64.encode(format!("ossup:{token}")));
    let _ = TUNNEL.set(Tunnel { port, token });

    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[direct] 隧道启动失败: {e}");
                return;
            }
        };
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                continue;
            };
            let expected = expected.clone();
            tokio::spawn(async move {
                if let Err(e) = serve(conn, &expected).await {
                    eprintln!("[direct] {e}");
                }
            });
        }
    });
    Ok(())
}

/// 给 ossutil 的 `--proxy`。隧道没起来就返回 None，调用方照常直接跑。
///
/// 口令只防同机其它进程白用这个隧道；它出现在命令行里，不是机密。
pub fn proxy_url() -> Option<String> {
    TUNNEL
        .get()
        .map(|t| format!("http://ossup:{}@127.0.0.1:{}", t.token, t.port))
}

fn random_token() -> String {
    format!("{:016x}{:016x}", rand_u64(), rand_u64())
}

/// RandomState 每个实例都带新的随机种子，够当一次性口令和 DNS 事务号，不用为此引 rand
fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

/* ------------------------------------------------------------------ */
/* CONNECT tunnel                                                      */
/* ------------------------------------------------------------------ */

async fn serve(mut client: TcpStream, expected_auth: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let head_end = loop {
        let mut chunk = [0u8; 1024];
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return reply(&mut client, "431 Request Header Fields Too Large").await;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let Some((host, port)) = parse_connect(&head) else {
        return reply(&mut client, "405 Method Not Allowed").await;
    };
    if !has_auth(&head, expected_auth) {
        return reply(&mut client, "407 Proxy Authentication Required").await;
    }

    let mut upstream = match dial(&host, port).await {
        Ok(s) => s,
        Err(e) => {
            let _ = reply(&mut client, "502 Bad Gateway").await;
            return Err(io::Error::new(e.kind(), format!("{host}:{port}: {e}")));
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    // CONNECT 头后面要是已经跟了数据（一般不会），原样转过去
    let rest = &buf[head_end..];
    if !rest.is_empty() {
        upstream.write_all(rest).await?;
    }
    let _ = tokio::io::copy_bidirectional_with_sizes(&mut client, &mut upstream, 64 * 1024, 64 * 1024)
        .await;
    Ok(())
}

async fn reply(client: &mut TcpStream, status: &str) -> io::Result<()> {
    client
        .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .await
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// `CONNECT bucket.oss-cn-hangzhou.aliyuncs.com:443 HTTP/1.1` → (host, 443)
fn parse_connect(head: &str) -> Option<(String, u16)> {
    let mut parts = head.lines().next()?.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let target = parts.next()?;
    let (host, port) = target.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

fn has_auth(head: &str, expected: &str) -> bool {
    head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("proxy-authorization") && v.trim() == expected
        })
    })
}

/// 绑网卡 IP + 真实 DNS 去连。找不到网卡 IP（没联网、不支持的平台）就退回普通
/// 连接 —— 至少不比不开这个功能更差。
async fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
    let source = tokio::task::spawn_blocking(source_ip).await.ok().flatten();
    let Some(src) = source else {
        return tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接超时"))?;
    };
    let ip = resolve(host, src).await?;
    let sock = TcpSocket::new_v4()?;
    sock.bind(SocketAddr::new(IpAddr::V4(src), 0))?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, sock.connect(SocketAddr::new(IpAddr::V4(ip), port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接超时"))??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/* ------------------------------------------------------------------ */
/* DNS                                                                 */
/* ------------------------------------------------------------------ */

/// TUN 代理的 fake-ip 段（RFC 2544 benchmark 段，Clash / sing-box 都用它）
fn is_fake(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

async fn resolve(host: &str, src: Ipv4Addr) -> io::Result<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let key = host.to_ascii_lowercase();
    static CACHE: OnceLock<Mutex<HashMap<String, (Ipv4Addr, Instant)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&(ip, at)) = cache.lock().unwrap().get(&key) {
        if at.elapsed() < DNS_TTL {
            return Ok(ip);
        }
    }

    let mut found = None;
    for server in RESOLVERS {
        if let Ok(Some(ip)) = query_a(&key, src, server).await {
            found = Some(ip);
            break;
        }
    }
    // 公共 DNS 全被墙掉的网络（公司内网常见）：退回系统 DNS，只要不是 fake-ip 就能用
    if found.is_none() {
        if let Ok(addrs) = tokio::net::lookup_host((key.as_str(), 0)).await {
            found = addrs
                .filter_map(|a| match a.ip() {
                    IpAddr::V4(v4) if !is_fake(v4) => Some(v4),
                    _ => None,
                })
                .next();
        }
    }

    let ip = found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("无法解析 {host}")))?;
    cache.lock().unwrap().insert(key, (ip, Instant::now()));
    Ok(ip)
}

async fn query_a(host: &str, src: Ipv4Addr, server: Ipv4Addr) -> io::Result<Option<Ipv4Addr>> {
    let id = rand_u64() as u16;
    let query = build_query(id, host).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "域名不合法"))?;
    let sock = UdpSocket::bind((src, 0)).await?;
    sock.connect((server, 53)).await?;
    sock.send(&query).await?;
    let mut buf = [0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.recv(&mut buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS 超时"))??;
    Ok(parse_answer(&buf[..n], id))
}

fn build_query(id: u16, host: &str) -> Option<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + host.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD，1 个问题
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // 根、A、IN
    Some(q)
}

/// 取回答区里第一条 A 记录。CNAME 链上的中间记录直接跳过。
fn parse_answer(msg: &[u8], id: u16) -> Option<Ipv4Addr> {
    if msg.len() < 12 || u16::from_be_bytes([msg[0], msg[1]]) != id {
        return None;
    }
    if msg[3] & 0x0f != 0 {
        return None; // rcode != NOERROR
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(msg, i)? + 4;
    }
    for _ in 0..an {
        i = skip_name(msg, i)?;
        let rr = msg.get(i..i + 10)?;
        let ty = u16::from_be_bytes([rr[0], rr[1]]);
        let len = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        i += 10;
        let data = msg.get(i..i + len)?;
        if ty == 1 && len == 4 {
            let ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
            if !is_fake(ip) {
                return Some(ip);
            }
        }
        i += len;
    }
    None
}

fn skip_name(msg: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *msg.get(i)?;
        if len & 0xc0 == 0xc0 {
            return Some(i + 2); // 压缩指针，名字到此结束
        }
        i += 1;
        if len == 0 {
            return Some(i);
        }
        i += len as usize;
    }
}

/* ------------------------------------------------------------------ */
/* 物理网卡 IP                                                          */
/* ------------------------------------------------------------------ */

/// 物理网卡上默认路由对应的本机 IPv4。结果缓存 `SOURCE_TTL`。
pub fn source_ip() -> Option<Ipv4Addr> {
    type Cached = Option<(Option<Ipv4Addr>, Instant)>;
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((ip, at)) = *cache.lock().unwrap() {
        if at.elapsed() < SOURCE_TTL {
            return ip;
        }
    }
    let ip = detect_source().filter(|ip| !is_fake(*ip) && !ip.is_loopback() && !ip.is_link_local());
    *cache.lock().unwrap() = Some((ip, Instant::now()));
    ip
}

/// Linux：TUN 的 auto-route 把默认路由放在自己的策略表里，main 表
/// （也就是 /proc/net/route）里留着的仍是物理网关。对网关「连」一下 UDP，
/// 内核选出来的源地址就是那块网卡的 IP —— 网关在直连网段上，不会被 TUN 截走。
#[cfg(target_os = "linux")]
fn detect_source() -> Option<Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    let gateway = default_gateway_linux(&table)?;
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // 端口随便挑，只是不能是 53 —— TUN 的规则会把 53 端口单独劫持进去
    sock.connect((gateway, 9)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}

#[cfg(target_os = "linux")]
fn default_gateway_linux(table: &str) -> Option<Ipv4Addr> {
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            if flags & (RTF_UP | RTF_GATEWAY) != RTF_UP | RTF_GATEWAY {
                return None;
            }
            // /proc 里是主机字节序（小端）的十六进制
            let gw = Ipv4Addr::from(u32::from_str_radix(f[2], 16).ok()?.swap_bytes());
            let metric: u32 = f[6].parse().ok()?;
            Some((metric, gw))
        })
        .min_by_key(|(m, _)| *m)
        .map(|(_, gw)| gw)
}

/// Windows：TUN 会加一条 metric 更低的默认路由，但物理网卡那条还在。
/// `route print` 的数字列不随系统语言变化，直接取「接口」列。
/// Windows 发包是强主机模型，绑了源地址就从那块网卡出去。
#[cfg(windows)]
fn detect_source() -> Option<Ipv4Addr> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("route")
        .args(["print", "-4", "0.0.0.0"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    default_iface_windows(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(any(windows, test))]
fn default_iface_windows(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() != 5 || f[0] != "0.0.0.0" || f[1] != "0.0.0.0" {
                return None;
            }
            // 网关得是个真 IP；TUN 的默认路由一般是「在链路上」
            f[2].parse::<Ipv4Addr>().ok()?;
            let iface: Ipv4Addr = f[3].parse().ok()?;
            let metric: u32 = f[4].parse().ok()?;
            (!is_fake(iface)).then_some((metric, iface))
        })
        .min_by_key(|(m, _)| *m)
        .map(|(_, ip)| ip)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn detect_source() -> Option<Ipv4Addr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line() {
        assert_eq!(
            parse_connect("CONNECT b.oss-cn-zhangjiakou.aliyuncs.com:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("b.oss-cn-zhangjiakou.aliyuncs.com".into(), 443))
        );
        assert_eq!(parse_connect("GET http://a/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_connect("CONNECT nohost HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn auth_header_is_case_insensitive() {
        let head = "CONNECT a:443 HTTP/1.1\r\nproxy-authorization: Basic abc\r\n\r\n";
        assert!(has_auth(head, "Basic abc"));
        assert!(!has_auth(head, "Basic xyz"));
    }

    #[test]
    fn dns_roundtrip_with_cname_and_compression() {
        let id = 0x1234;
        let mut msg = build_query(id, "b.example.com").unwrap();
        msg[2] = 0x81;
        msg[3] = 0x80; // QR + RD + RA, NOERROR
        msg[7] = 2; // 2 条回答
        // CNAME：名字是指向问题区的压缩指针 0xc00c
        msg.extend_from_slice(&[0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 0x0c]);
        // A 47.92.17.55
        msg.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 47, 92, 17, 55]);
        assert_eq!(parse_answer(&msg, id), Some(Ipv4Addr::new(47, 92, 17, 55)));
        assert_eq!(parse_answer(&msg, id + 1), None);
    }

    #[test]
    fn dns_rejects_fake_ip_and_errors() {
        let id = 7;
        let mut msg = build_query(id, "a.b").unwrap();
        msg[7] = 1;
        msg.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 198, 18, 0, 31]);
        assert_eq!(parse_answer(&msg, id), None);
        msg[3] = 0x03; // NXDOMAIN
        assert_eq!(parse_answer(&msg, id), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_gateway_picks_lowest_metric() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlp0s20f3\t00000000\t01CCA8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            enp0s31f6\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            wlp0s20f3\t00CCA8C0\t00000000\t0001\t0\t0\t600\t00FEFFFF\t0\t0\t0\n";
        assert_eq!(default_gateway_linux(table), Some(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn windows_route_print_skips_tun() {
        let text = "\
IPv4 Route Table
===========================================================================
Active Routes:
Network Destination        Netmask          Gateway       Interface  Metric
          0.0.0.0          0.0.0.0         On-link       198.18.0.1      0
          0.0.0.0          0.0.0.0      192.168.1.1    192.168.1.23     35
===========================================================================";
        assert_eq!(default_iface_windows(text), Some(Ipv4Addr::new(192, 168, 1, 23)));
    }
}
